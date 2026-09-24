//! `cce-list` — small lists of things to remember, kept on the desktop.
//!
//! A plain floating window: the compositor saves and restores it across
//! sessions (position, size, and respawn) like any other app, and in overview
//! mode it takes the normal move/resize ring. The title band is a dropdown
//! naming the current list; it switches between lists and carries two
//! trailing entries, "New list…" and "Delete list…", which turn the input
//! box into a name prompt or a confirmation. One `TextBox` adds items; a
//! click on a row toggles it done; the ✕ that appears on hover deletes it.
//! Rows scroll when they outgrow the window.
//!
//! Each list is a plain markdown checklist on disk
//! (`~/.local/share/cce-list/lists/<title>.md`), so it can be read and
//! edited with anything; the shown list is named in a `current` file next
//! to them. Lists and items mirrored from Google Tasks by `cce-list-sync`
//! carry `<!-- list:… -->` / `<!-- uid:… -->` comments; toggling, adding,
//! deleting — items or whole lists — here is pushed to the server on the
//! next sync tick, and the app re-reads the directory when the sync (or a
//! hand edit) changes it.

use cce_list::{
    delete_list, lists_dir, load_current, load_lists, save_current, save_list, Item, ListFile,
};
use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{Cap, DisplayList, PaintCtx};
use cce_ui::widget::{
    Adapted, Bounds, Dropdown, ElementState, Event, Key, KeyEvent, MouseButton, MouseScrollDelta,
    NamedKey, ScrollMotion, TextBox, WidgetHost,
};
use wayland_client::QueueHandle;

/// Initial size only — the window is freely resizable and the compositor
/// restores the last geometry across sessions.
const INIT_W: u32 = 300;
const INIT_H: u32 = 320;
/// Small enough that the title band, the input box, and one row stay usable.
const MIN_SIZE: (u32, u32) = (220, 160);
const ROW_H: f32 = 26.0;
const INPUT_H: f32 = 30.0;
const TITLE_FONT_SIZE: f32 = 14.0;
/// The list switcher in the title band: its height, and the share of the
/// band's width it takes — the rest stays a drag handle for the window.
const SWITCHER_H: f32 = 24.0;
const SWITCHER_SHARE: f32 = 0.62;
/// Checkbox disc radius; its hit target is the whole row, this is only drawn.
/// The mark itself is cce-ui's round `Checkbox` style, so it matches one.
const CHECK_R: f32 = cce_ui::widget::Checkbox::ROUND_RADIUS;
/// Side of the ✕ delete target at a row's right edge.
const DELETE_S: f32 = 18.0;
/// How often the lists directory is re-read for outside changes (the sync
/// timer, a hand edit). The runner wakes an idle app once a second by itself,
/// so this costs no extra frames; `idle_poll_interval` pins the cadence
/// rather than inheriting it.
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// The switcher's trailing pseudo-entries, after the list titles.
const NEW_LIST: &str = "New list…";
const DELETE_LIST: &str = "Delete list…";
const ITEM_PLACEHOLDER: &str = "Remember to…";

#[derive(Debug, Clone)]
enum ListMessage {
    Exit,
}

/// What the input box is for right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Typing adds an item to the current list.
    Items,
    /// Typing names a new list; Enter creates and shows it.
    NamingList,
    /// Enter deletes the current list, Escape keeps it.
    ConfirmDelete,
}

// ── Layout: hand math over a fixed-width column ───────────────────────────

/// The frame's fixed vertical anatomy, derived once per use from the shared
/// config paddings so paint, hit-testing, and `desired_size` cannot drift.
struct Metrics {
    pad: f32,
    band_h: f32,
    switcher: Rect,
    input: Rect,
    list_top: f32,
}

fn text_leaf_height(font_size: f32) -> f32 {
    (font_size * 1.2).ceil()
}

fn metrics(width: f32) -> Metrics {
    // The window-edge inset: the root plate's roll plus one padding.
    let pad = cce_ui::layout::root_plate_inset();
    let band_h = pad + text_leaf_height(TITLE_FONT_SIZE) + 10.0;
    let gap = cce_ui::layout::bevel_width().max(4.0);
    let input_y = band_h + gap;
    Metrics {
        pad,
        band_h,
        switcher: Rect {
            x: pad,
            y: ((band_h - SWITCHER_H) / 2.0).max(2.0),
            width: ((width - 2.0 * pad) * SWITCHER_SHARE).max(80.0),
            height: SWITCHER_H,
        },
        input: Rect { x: pad, y: input_y, width: width - 2.0 * pad, height: INPUT_H },
        list_top: input_y + INPUT_H + gap,
    }
}

fn srgb_u8(linear: [f32; 4]) -> [u8; 3] {
    let srgb = cce_ui::colors::to_srgb(linear);
    [
        (srgb[0] * 255.0) as u8,
        (srgb[1] * 255.0) as u8,
        (srgb[2] * 255.0) as u8,
    ]
}

/// A cheap fingerprint of the lists directory and the `current` pointer:
/// names and mtimes. Compared each second; a change means something else
/// wrote there and the app re-reads.
fn disk_signature() -> Vec<(String, Option<std::time::SystemTime>)> {
    let mut sig = Vec::new();
    if let Ok(entries) = std::fs::read_dir(lists_dir()) {
        for e in entries.flatten() {
            let mtime = e.metadata().and_then(|m| m.modified()).ok();
            sig.push((e.file_name().to_string_lossy().into_owned(), mtime));
        }
    }
    sig.push((
        "current".to_string(),
        std::fs::metadata(cce_list::current_path()).and_then(|m| m.modified()).ok(),
    ));
    sig.sort();
    sig
}

// ── Application ───────────────────────────────────────────────────────────

struct ListApp {
    /// Every list on disk, sorted by title; `cur` indexes the shown one.
    /// Every mutation saves before the frame that shows it.
    lists: Vec<ListFile>,
    cur: usize,
    mode: Mode,
    switcher: Adapted<Dropdown>,
    input_box: Adapted<TextBox>,
    ui_context: cce_ui::context::UiContext,
    width: u32,
    height: u32,
    scale_factor: f64,
    needs_rebuild: bool,
    widgets_registered: bool,
    /// How far the list is scrolled down, in logical px; non-zero only once
    /// the rows overflow the window. The DRAWN offset — `scroll_motion`
    /// glides it (wheel) or coasts it (trackpad flick); direct writes (End
    /// key, clamp) are adopted by the motion on its next step.
    scroll: f32,
    scroll_motion: ScrollMotion,
    pointer: Option<(f32, f32)>,
    hovered_row: Option<usize>,
    /// Outside-change detection: what the directory looked like when the
    /// lists were last read, and the countdown to the next look.
    disk_sig: Vec<(String, Option<std::time::SystemTime>)>,
    /// When the directory may be re-read again. A wall clock, not an
    /// accumulation of `tick`'s `dt`: `dt` is animation time, clamped to one
    /// frame after an idle sleep, and a list nobody is typing into is idle —
    /// so the once-a-second look actually happened about once a minute.
    watch_at: std::time::Instant,
}

impl ListApp {
    fn items(&self) -> &[Item] {
        self.lists.get(self.cur).map(|l| l.items.as_slice()).unwrap_or(&[])
    }

    fn switcher_options(lists: &[ListFile]) -> Vec<String> {
        lists
            .iter()
            .map(|l| l.title.clone())
            .chain([NEW_LIST.to_string(), DELETE_LIST.to_string()])
            .collect()
    }

    /// (Re)read every list from disk. Keeps the shown list by title where it
    /// still exists (the sync may have renamed or removed it), guarantees at
    /// least one list, and refreshes the switcher.
    fn load_from_disk(&mut self) {
        let mut lists = match load_lists() {
            Ok(l) => l,
            Err(e) => {
                log::error!("cce-list: reading lists: {e}");
                Vec::new()
            }
        };
        if lists.is_empty() {
            let first = ListFile { title: "Tasks".to_string(), id: None, items: Vec::new() };
            if let Err(e) = save_list(&first) {
                log::error!("cce-list: creating the first list: {e}");
            }
            lists.push(first);
        }
        let wanted = load_current().or_else(|| self.lists.get(self.cur).map(|l| l.title.clone()));
        let cur = wanted
            .as_deref()
            .and_then(|t| lists.iter().position(|l| l.title == t))
            .unwrap_or(0);
        if wanted.as_deref() != Some(lists[cur].title.as_str()) {
            let _ = save_current(&lists[cur].title);
        }
        self.lists = lists;
        self.cur = cur;
        self.switcher.options = Self::switcher_options(&self.lists);
        self.switcher.selected = cur;
        self.disk_sig = disk_signature();
        self.clamp_scroll();
        if let Some((px, py)) = self.pointer {
            self.hovered_row = self.row_at(px, py);
        }
        self.needs_rebuild = true;
    }

    fn save_current_list(&mut self) {
        if let Some(list) = self.lists.get(self.cur) {
            if let Err(e) = save_list(list) {
                log::error!("cce-list: failed to save {}: {e}", list.title);
            }
        }
        // Our own write must not read as an outside change next tick.
        self.disk_sig = disk_signature();
    }

    fn select_list(&mut self, idx: usize) {
        if idx >= self.lists.len() {
            return;
        }
        self.cur = idx;
        self.switcher.selected = idx;
        if let Err(e) = save_current(&self.lists[idx].title) {
            log::error!("cce-list: saving current list: {e}");
        }
        self.disk_sig = disk_signature();
        self.scroll = 0.0;
        self.hovered_row = None;
        self.set_mode(Mode::Items);
        self.needs_rebuild = true;
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        let placeholder = match mode {
            Mode::Items => ITEM_PLACEHOLDER.to_string(),
            Mode::NamingList => "Name the new list, then Enter".to_string(),
            Mode::ConfirmDelete => format!(
                "Enter deletes “{}” · Esc keeps it",
                self.lists.get(self.cur).map(|l| l.title.as_str()).unwrap_or("")
            ),
        };
        self.input_box.set_placeholder(&placeholder);
        self.clear_input();
    }

    fn clear_input(&mut self) {
        self.input_box.text.clear();
        self.input_box.edit_buffer.clear();
        self.input_box.cursor_idx = 0;
    }

    /// The live value: while the box is in edit mode the typed text sits in
    /// `edit_buffer`; `text` is only the last committed value.
    fn input_value(&self) -> String {
        let raw = if self.input_box.editing {
            &self.input_box.edit_buffer
        } else {
            &self.input_box.text
        };
        raw.trim().to_string()
    }

    fn begin_new_list(&mut self) {
        self.set_mode(Mode::NamingList);
        self.input_box.focus();
        self.needs_rebuild = true;
    }

    fn create_list(&mut self, title: &str) {
        let title = cce_list::safe_title(title);
        if let Some(idx) = self.lists.iter().position(|l| l.title == title) {
            // Already there: just show it.
            self.select_list(idx);
            return;
        }
        let list = ListFile { title: title.clone(), id: None, items: Vec::new() };
        if let Err(e) = save_list(&list) {
            log::error!("cce-list: creating {title}: {e}");
            return;
        }
        self.lists.push(list);
        self.lists.sort_by_key(|l| l.title.to_lowercase());
        self.switcher.options = Self::switcher_options(&self.lists);
        let idx = self.lists.iter().position(|l| l.title == title).unwrap_or(0);
        self.select_list(idx);
    }

    fn begin_delete(&mut self) {
        if self.lists.len() <= 1 {
            // The server keeps a default list too; one is the floor.
            self.set_mode(Mode::Items);
            self.input_box.set_placeholder("Keep at least one list");
            self.needs_rebuild = true;
            return;
        }
        self.set_mode(Mode::ConfirmDelete);
        self.input_box.unfocus();
        self.needs_rebuild = true;
    }

    fn confirm_delete(&mut self) {
        let Some(list) = self.lists.get(self.cur) else { return };
        let title = list.title.clone();
        if let Err(e) = delete_list(&title) {
            log::error!("cce-list: deleting {title}: {e}");
            self.set_mode(Mode::Items);
            return;
        }
        self.lists.remove(self.cur);
        self.switcher.options = Self::switcher_options(&self.lists);
        let idx = self.cur.min(self.lists.len().saturating_sub(1));
        self.select_list(idx);
    }

    /// The switcher reported a pick: a list, or one of the two actions.
    fn switcher_picked(&mut self) {
        let idx = self.switcher.selected;
        if idx < self.lists.len() {
            if idx != self.cur {
                self.select_list(idx);
            }
        } else {
            // A pseudo-entry: restore the trigger to the shown list.
            self.switcher.selected = self.cur;
            if idx == self.lists.len() {
                self.begin_new_list();
            } else {
                self.begin_delete();
            }
        }
        self.needs_rebuild = true;
    }

    fn list_viewport(&self, m: &Metrics) -> Rect {
        Rect {
            x: 0.0,
            y: m.list_top,
            width: self.width as f32,
            height: (self.height as f32 - m.list_top - m.pad).max(0.0),
        }
    }

    /// Row `i`'s rect in window coordinates, scroll applied.
    fn row_rect(&self, m: &Metrics, i: usize) -> Rect {
        Rect {
            x: m.pad,
            y: m.list_top + i as f32 * ROW_H - self.scroll,
            width: self.width as f32 - 2.0 * m.pad,
            height: ROW_H,
        }
    }

    fn delete_rect(row: Rect) -> Rect {
        Rect {
            x: row.x + row.width - DELETE_S,
            y: row.y + (row.height - DELETE_S) / 2.0,
            width: DELETE_S,
            height: DELETE_S,
        }
    }

    fn max_scroll(&self, m: &Metrics) -> f32 {
        (self.items().len() as f32 * ROW_H - self.list_viewport(m).height).max(0.0)
    }

    fn clamp_scroll(&mut self) {
        let m = metrics(self.width as f32);
        self.scroll = self.scroll.clamp(0.0, self.max_scroll(&m));
    }

    /// Re-derive what depends on the drawn offset after it moved.
    fn after_scroll_moved(&mut self) {
        if let Some((px, py)) = self.pointer {
            self.hovered_row = self.row_at(px, py);
        }
    }

    /// Advance the wheel glide / flick coast; true while the offset moved.
    fn tick_scroll(&mut self, dt: f32) -> bool {
        self.scroll_motion.reconcile(0.0, self.scroll);
        if !self.scroll_motion.is_animating() {
            return false;
        }
        let m = metrics(self.width as f32);
        let max = self.max_scroll(&m);
        let moved = self.scroll_motion.tick(dt, Bounds::max(0.0), Bounds::max(max));
        self.scroll = self.scroll_motion.y.pos();
        if moved {
            self.after_scroll_moved();
        }
        moved || self.scroll_motion.is_animating()
    }

    fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        let m = metrics(self.width as f32);
        let vp = self.list_viewport(&m);
        if y < vp.y || y > vp.y + vp.height {
            return None;
        }
        let i = ((y - m.list_top + self.scroll) / ROW_H).floor();
        let row = (i >= 0.0).then_some(i as usize).filter(|&i| i < self.items().len())?;
        let r = self.row_rect(&m, row);
        (x >= r.x && x <= r.x + r.width).then_some(row)
    }

    fn submit_input(&mut self) {
        let text = self.input_value();
        match self.mode {
            Mode::ConfirmDelete => self.confirm_delete(),
            Mode::NamingList => {
                if !text.is_empty() {
                    self.create_list(&text);
                }
            }
            Mode::Items => {
                if text.is_empty() {
                    return;
                }
                if let Some(list) = self.lists.get_mut(self.cur) {
                    list.items.push(Item { text, done: false, uid: None });
                }
                self.clear_input();
                self.save_current_list();
                // Keep the fresh item in view once the window is at its height cap.
                let m = metrics(self.width as f32);
                self.scroll = self.max_scroll(&m);
                self.needs_rebuild = true;
            }
        }
    }
}

impl Application for ListApp {
    type Message = ListMessage;

    fn new(
        _qh: &QueueHandle<EngineState<Self>>,
        _sender: calloop::channel::Sender<Self::Message>,
    ) -> Self {
        cce_ui::scale::set_scale_factor(1.0);
        let mut app = Self {
            lists: Vec::new(),
            cur: 0,
            mode: Mode::Items,
            switcher: Dropdown::new(Vec::new(), 0),
            input_box: TextBox::new(String::new()).with_placeholder(ITEM_PLACEHOLDER),
            ui_context: cce_ui::context::UiContext::new(),
            width: INIT_W,
            height: INIT_H,
            scale_factor: 1.0,
            needs_rebuild: true,
            widgets_registered: false,
            scroll: 0.0,
            scroll_motion: ScrollMotion::new(),
            pointer: None,
            hovered_row: None,
            disk_sig: Vec::new(),
            watch_at: std::time::Instant::now(),
        };
        app.load_from_disk();
        app
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "cce-list".to_string(),
            app_id: "cce-list".to_string(),
            width: INIT_W,
            height: INIT_H,
            fullscreen: false,
            min_size: Some(MIN_SIZE),
        }
    }

    fn update(&mut self, msg: Self::Message, _needs_rebuild: &mut bool, exit: &mut bool) {
        match msg {
            ListMessage::Exit => *exit = true,
        }
    }

    /// The directory watch in `tick` is work the runner cannot see — nothing
    /// redraws until the files change underneath us — so name the cadence the
    /// loop has to come back at.
    fn idle_poll_interval(&self) -> Option<std::time::Duration> {
        Some(WATCH_EVERY)
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.ui_context.tick(dt) {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
        if self.tick_scroll(dt) {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
        // Outside changes (the sync tick, a hand edit) show up without a
        // relaunch — but never while typing a name, which a reload would
        // interrupt; that waits a second.
        let now = std::time::Instant::now();
        if now >= self.watch_at {
            self.watch_at = now + WATCH_EVERY;
            if self.mode == Mode::Items && disk_signature() != self.disk_sig {
                self.load_from_disk();
                *needs_rebuild = true;
            }
        }
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<DisplayList> {
        // Register once, at self's final address (the registry stores pointers).
        if !self.widgets_registered {
            self.widgets_registered = true;
            let (id, ptr) = (self.input_box.id(), self.input_box.as_ptr_mut());
            self.ui_context.register_widget(id, ptr);
            let (id, ptr) = (self.switcher.id(), self.switcher.as_ptr_mut());
            self.ui_context.register_widget(id, ptr);
        }

        let size_changed = self.width != size.width as u32
            || self.height != size.height as u32
            || self.scale_factor != scale;
        if size_changed {
            self.width = size.width as u32;
            self.height = size.height as u32;
            self.scale_factor = scale;
            cce_ui::scale::set_scale_factor(scale as f32);
            self.clamp_scroll();
        }
        let m = metrics(self.width as f32);
        if self.needs_rebuild || size_changed {
            self.input_box
                .set_rect(m.input.x, m.input.y, m.input.width, m.input.height);
            self.switcher
                .set_rect(m.switcher.x, m.switcher.y, m.switcher.width, m.switcher.height);
            self.needs_rebuild = false;
            self.ui_context.rebuild_spatial_grid();
        }
        // An open menu overlays the rows: registered as a popover so it is
        // hit-tested above them and clips the row text beneath; the popover
        // pass at the end of this function draws it. Re-registered every
        // frame from a clean slate, since the rect animates and closes.
        self.ui_context.clear_popovers();
        if self.switcher.popover_rect().is_some() {
            self.ui_context.register_popover(&mut self.switcher);
        }

        let (w, h) = (self.width as f32, self.height as f32);
        let mut pc = PaintCtx::new();

        // The standard root plate, then the title strip carved one step down
        // into it (the recessed header idiom — its only wall faces the content).
        pc.root_plate(w, h);
        pc.recess_edges(
            Rect { x: 0.0, y: 0.0, width: w, height: m.band_h },
            (0.0, 0.0, 0.0, 0.0),
            cce_ui::layout::bar_wall_width(),
            (false, false, true, false),
        );

        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.input_box, &mut pc);

        // The rows, clipped to the viewport so a scrolled list never bleeds
        // into the input or the plate's bottom roll.
        let (family, font_size) = cce_ui::layout::list_font_parsed();
        let vp = self.list_viewport(&m);
        let items = self.items();
        pc.clip(vp, |pc| {
            if items.is_empty() {
                let r = self.row_rect(&m, 0);
                pc.text_with(
                    "nothing to remember".to_string(),
                    r.x,
                    cce_ui::layout::align_text_y(r.y, r.height, font_size, 0.0),
                    font_size,
                    srgb_u8(cce_ui::colors::TEXT_DIM),
                    Some(family.clone()),
                    None,
                );
            }
            for (i, item) in items.iter().enumerate() {
                let r = self.row_rect(&m, i);
                if r.y + r.height < vp.y || r.y > vp.y + vp.height {
                    continue;
                }
                let hovered = self.hovered_row == Some(i);
                let (cx, cy) = (r.x + CHECK_R, r.y + r.height / 2.0);
                cce_ui::widget::Checkbox::paint_round_mark(pc, cx, cy, CHECK_R, item.done);
                let color = if item.done {
                    cce_ui::colors::TEXT_DIM
                } else {
                    cce_ui::colors::TEXT_FG
                };
                let text_x = cx + CHECK_R + 8.0;
                // The ✕ zone bounds the label whether or not it is drawn, so
                // hovering never truncates the text it just revealed the ✕ over.
                let text_end = r.x + r.width - DELETE_S - 4.0;
                let text_y = cce_ui::layout::align_text_y(r.y, r.height, font_size, 0.0);
                pc.text_with(
                    item.text.clone(),
                    text_x,
                    text_y,
                    font_size,
                    srgb_u8(color),
                    Some(family.clone()),
                    Some([text_x, r.y, text_end, r.y + r.height]),
                );
                if item.done {
                    // Struck through rather than restyled: the row stays
                    // readable, it just reads as handled.
                    let strike_w = ((item.text.chars().count() as f32) * font_size * 0.55)
                        .min(text_end - text_x);
                    pc.vector(
                        text_x,
                        cy,
                        text_x + strike_w,
                        cy,
                        1.0,
                        cce_ui::colors::TEXT_DIM,
                        Cap::Flat,
                    );
                }
                if hovered {
                    let d = Self::delete_rect(r);
                    let (dcx, dcy) = (d.x + d.width / 2.0, d.y + d.height / 2.0);
                    let arm = 4.0;
                    let dim = cce_ui::colors::TEXT_DIM;
                    pc.vector(dcx - arm, dcy - arm, dcx + arm, dcy + arm, 1.5, dim, Cap::Round);
                    pc.vector(dcx - arm, dcy + arm, dcx + arm, dcy - arm, 1.5, dim, Cap::Round);
                }
            }
        });

        // The switcher's trigger, then its open menu on top of everything.
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.switcher, &mut pc);
        self.switcher.render_popover(&mut pc);

        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn ui_context(&self) -> Option<&cce_ui::context::UiContext> {
        Some(&self.ui_context)
    }

    fn ui_context_mut(&mut self) -> Option<&mut cce_ui::context::UiContext> {
        Some(&mut self.ui_context)
    }

    /// Drag the window by the title band, right of the switcher; everywhere
    /// else is content.
    fn is_movable_root_plate_at(&self, px: f32, py: f32) -> bool {
        let m = metrics(self.width as f32);
        py <= m.band_h && px > m.switcher.x + m.switcher.width
    }

    fn clear_color(&self) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        self.pointer = Some((pos.x, pos.y));
        let ev = Event::PointerMove { x: pos.x, y: pos.y, local_x: pos.x, local_y: pos.y };
        if self.ui_context.propagate_event(&ev, self.switcher.id()) {
            *needs_rebuild = true;
        }
        // Rows under an open menu are not hoverable.
        let hovered = if self.switcher.open { None } else { self.row_at(pos.x, pos.y) };
        if hovered != self.hovered_row {
            self.hovered_row = hovered;
            *needs_rebuild = true;
        }
        if self.ui_context.propagate_event(&ev, self.input_box.id()) {
            *needs_rebuild = true;
        }
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        let (px, py) = (pos.x, pos.y);
        let ev = Event::MouseButton { button, state, x: px, y: py, local_x: px, local_y: py };

        // The switcher routes first: its open menu overlays the rows, so a
        // press it handles must not fall through to what is beneath.
        if self.ui_context.propagate_event(&ev, self.switcher.id()) {
            if self.switcher.take_change() {
                self.switcher_picked();
            }
            *needs_rebuild = true;
            self.needs_rebuild = true;
            return None;
        }

        if button == MouseButton::Left && state == ElementState::Pressed {
            if let Some(i) = self.row_at(px, py) {
                let m = metrics(self.width as f32);
                let d = Self::delete_rect(self.row_rect(&m, i));
                if let Some(list) = self.lists.get_mut(self.cur) {
                    if px >= d.x && px <= d.x + d.width && py >= d.y && py <= d.y + d.height {
                        list.items.remove(i);
                        self.clamp_scroll();
                        self.hovered_row = self.row_at(px, py);
                    } else {
                        list.items[i].done = !list.items[i].done;
                    }
                }
                self.save_current_list();
                self.needs_rebuild = true;
                *needs_rebuild = true;
                return None;
            }
        }
        if state == ElementState::Pressed && !self.input_box.hit_test(px, py, &self.ui_context) {
            self.input_box.unfocus();
            *needs_rebuild = true;
        }
        if self.ui_context.propagate_event(&ev, self.input_box.id()) {
            *needs_rebuild = true;
        }
        None
    }

    fn handle_mouse_wheel(
        &mut self,
        delta: &MouseScrollDelta,
        _pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) {
        let m = metrics(self.width as f32);
        let max = self.max_scroll(&m);
        if max <= 0.0 || self.switcher.open {
            return;
        }
        self.scroll_motion.reconcile(0.0, self.scroll);
        let moved = self.scroll_motion.apply(delta, (ROW_H, ROW_H), Bounds::max(0.0), Bounds::max(max));
        self.scroll = self.scroll_motion.y.pos();
        if moved {
            self.after_scroll_moved();
            self.needs_rebuild = true;
            *needs_rebuild = true;
        }
    }

    fn handle_key_input(
        &mut self,
        event: &KeyEvent,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        let ev = Event::KeyInput(event.clone());
        // An open menu takes the keyboard: arrows move, Enter picks.
        if self.switcher.open {
            if self.ui_context.propagate_event(&ev, self.switcher.id()) {
                if self.switcher.take_change() {
                    self.switcher_picked();
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
                return None;
            }
        }
        if event.state == ElementState::Pressed && !event.repeat {
            if event.ctrl {
                if let Key::Character(ref c) = event.logical_key {
                    if c == "q" {
                        return Some(ListMessage::Exit);
                    }
                }
            }
            if let Key::Named(NamedKey::Escape) = event.logical_key {
                self.input_box.unfocus();
                if self.mode != Mode::Items {
                    self.set_mode(Mode::Items);
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
                return None;
            }
            if let Key::Named(NamedKey::Enter) = event.logical_key {
                // The box's own edit mode, not `focused(&ctx)`: a click focuses
                // through the thread-local focus registry, so the UiContext's
                // focused_widget — which that checks — never learns of it.
                // A pending deletion takes Enter from anywhere.
                if self.input_box.editing || self.mode == Mode::ConfirmDelete {
                    self.submit_input();
                    *needs_rebuild = true;
                    self.needs_rebuild = true;
                    return None;
                }
            }
        }
        if self.ui_context.propagate_event(&ev, self.input_box.id()) {
            *needs_rebuild = true;
        }
        None
    }
}

fn main() {
    env_logger::init();
    cce_ui::engine::run::<ListApp>();
}
