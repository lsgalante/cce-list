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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cce_vault::index::stem;
use cce_vault::{Index, Task, TaskPlace, VaultWatcher};

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

    /// Move the task in row `from` to the gap before row `slot` (a drag's
    /// drop; `slot` may be one past the last row). It goes just before the
    /// task under the gap, or else just after the task over it — so into
    /// whichever note's group the gap belongs to, under that note's name
    /// when dropped right below it. Tasks the view does not show (done ones)
    /// stay where they are in their notes.
    pub fn move_row(&mut self, from: usize, slot: usize) -> Result<(), String> {
        let Some(Row::Task { path, line, .. }) = self.rows.get(from).cloned() else { return Ok(()) };
        let task_at = |i: usize| match self.rows.get(i) {
            Some(Row::Task { path, line, .. }) => Some((path.clone(), *line)),
            _ => None,
        };
        let dest = match (task_at(slot), slot.checked_sub(1).and_then(task_at)) {
            (Some((p, l)), _) => Some((p, TaskPlace::Before(l))),
            (None, Some((p, l))) => Some((p, TaskPlace::After(l))),
            // Above the first note's name: before its first task.
            (None, None) => task_at(slot + 1).map(|(p, l)| (p, TaskPlace::Before(l))),
        };
        let Some((to, place)) = dest else { return Ok(()) };
        self.index.move_task(&path, line, &to, place).map_err(|e| e.to_string())?;
        // Lines in both notes have shifted: this session's ticks there no
        // longer name the tasks they did.
        self.ticked.retain(|(p, _)| *p != path && *p != to);
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

    /// Show a note in cce-notes: hand it to the running instance, or start
    /// one. `notes_ipc::open` does it on a thread of its own, bounded: this
    /// is called from a click on the UI thread, and a cce-notes that took
    /// the connection but never answered froze the list.
    pub fn open_note(&self, path: &str) {
        let abs = self.index.abs(path);
        cce_vault::notes_ipc::open(&abs, None, |e| log::warn!("[vault-tasks] {e}"));
    }
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
