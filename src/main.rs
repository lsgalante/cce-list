//! `cce-list` — a small list of things to remember, kept on the desktop.
//!
//! A plain floating window: the compositor saves and restores it across
//! sessions (position, size, and respawn) like any other app, and in overview
//! mode it takes the normal move/resize ring. One `TextBox` adds items; a
//! click on a row toggles it done; the ✕ that appears on hover deletes it.
//! Rows scroll when they outgrow the window. The list is a plain markdown
//! checklist on disk (`~/.local/share/cce-list/list.md`), so it can be read
//! and edited with anything.

use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{Cap, DisplayList, PaintCtx, PlateSpec};
use cce_ui::widget::{
    Adapted, ElementState, Event, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey, TextBox,
    WidgetHost,
};
use std::path::PathBuf;
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
/// Checkbox disc radius; its hit target is the whole row, this is only drawn.
const CHECK_R: f32 = 7.0;
/// Side of the ✕ delete target at a row's right edge.
const DELETE_S: f32 = 18.0;

#[derive(Debug, Clone)]
enum ListMessage {
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    text: String,
    done: bool,
}

// ── Persistence: a markdown checklist ─────────────────────────────────────

fn data_path() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("cce-list/list.md")
}

/// Checklist lines become items; any other non-empty line is adopted as a
/// not-done item rather than parsed around — the next save rewrites the file,
/// so a line this reader skipped would be a line silently deleted.
fn parse_items(text: &str) -> Vec<Item> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let (done, rest) = if let Some(r) = trimmed.strip_prefix("- [ ] ") {
                (false, r)
            } else if let Some(r) = trimmed.strip_prefix("- [x] ").or_else(|| trimmed.strip_prefix("- [X] ")) {
                (true, r)
            } else {
                (false, trimmed)
            };
            Some(Item { text: rest.to_string(), done })
        })
        .collect()
}

fn serialize_items(items: &[Item]) -> String {
    items
        .iter()
        .map(|i| format!("- [{}] {}\n", if i.done { 'x' } else { ' ' }, i.text))
        .collect()
}

fn load_items() -> Vec<Item> {
    match std::fs::read_to_string(data_path()) {
        Ok(text) => parse_items(&text),
        Err(_) => Vec::new(),
    }
}

/// Write-temp-then-rename in the same directory, so a crash mid-write never
/// leaves a truncated list behind.
fn save_items(items: &[Item]) {
    let path = data_path();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, serialize_items(items))?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        log::error!("cce-list: failed to save {}: {e}", path.display());
    }
}

// ── Layout: hand math over a fixed-width column ───────────────────────────

/// The frame's fixed vertical anatomy, derived once per use from the shared
/// config paddings so paint, hit-testing, and `desired_size` cannot drift.
struct Metrics {
    pad: f32,
    band_h: f32,
    input: Rect,
    list_top: f32,
}

fn text_leaf_height(font_size: f32) -> f32 {
    (font_size * 1.2).ceil()
}

fn metrics(width: f32) -> Metrics {
    let pad = cce_ui::layout::root_plate_padding();
    let band_h = pad + text_leaf_height(TITLE_FONT_SIZE) + 10.0;
    let gap = cce_ui::layout::bevel_width().max(4.0);
    let input_y = band_h + gap;
    Metrics {
        pad,
        band_h,
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

// ── Application ───────────────────────────────────────────────────────────

struct ListApp {
    /// The source of truth; every mutation saves before the frame that shows it.
    items: Vec<Item>,
    input_box: Adapted<TextBox>,
    ui_context: cce_ui::context::UiContext,
    width: u32,
    height: u32,
    scale_factor: f64,
    needs_rebuild: bool,
    widgets_registered: bool,
    /// How far the list is scrolled down, in logical px; non-zero only once
    /// the rows overflow the window.
    scroll: f32,
    pointer: Option<(f32, f32)>,
    hovered_row: Option<usize>,
}

impl ListApp {
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
        (self.items.len() as f32 * ROW_H - self.list_viewport(m).height).max(0.0)
    }

    fn clamp_scroll(&mut self) {
        let m = metrics(self.width as f32);
        self.scroll = self.scroll.clamp(0.0, self.max_scroll(&m));
    }

    fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        let m = metrics(self.width as f32);
        let vp = self.list_viewport(&m);
        if y < vp.y || y > vp.y + vp.height {
            return None;
        }
        let i = ((y - m.list_top + self.scroll) / ROW_H).floor();
        let row = (i >= 0.0).then_some(i as usize).filter(|&i| i < self.items.len())?;
        let r = self.row_rect(&m, row);
        (x >= r.x && x <= r.x + r.width).then_some(row)
    }

    fn submit_input(&mut self) {
        // The live value: while the box is in edit mode the typed text sits in
        // `edit_buffer`; `text` is only the last committed value.
        let raw = if self.input_box.editing {
            &self.input_box.edit_buffer
        } else {
            &self.input_box.text
        };
        let text = raw.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.items.push(Item { text, done: false });
        self.input_box.text.clear();
        self.input_box.edit_buffer.clear();
        self.input_box.cursor_idx = 0;
        save_items(&self.items);
        // Keep the fresh item in view once the window is at its height cap.
        let m = metrics(self.width as f32);
        self.scroll = self.max_scroll(&m);
        self.needs_rebuild = true;
    }
}

impl Application for ListApp {
    type Message = ListMessage;

    fn new(
        _qh: &QueueHandle<EngineState<Self>>,
        _sender: calloop::channel::Sender<Self::Message>,
    ) -> Self {
        cce_ui::scale::set_scale_factor(1.0);
        Self {
            items: load_items(),
            input_box: TextBox::new(String::new()).with_placeholder("Remember to…"),
            ui_context: cce_ui::context::UiContext::new(),
            width: INIT_W,
            height: INIT_H,
            scale_factor: 1.0,
            needs_rebuild: true,
            widgets_registered: false,
            scroll: 0.0,
            pointer: None,
            hovered_row: None,
        }
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

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.ui_context.tick(dt) {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<DisplayList> {
        // Register once, at self's final address (the registry stores pointers).
        if !self.widgets_registered {
            self.widgets_registered = true;
            let ptr = self.input_box.as_ptr_mut();
            let id = self.input_box.id();
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
            self.needs_rebuild = false;
            self.ui_context.rebuild_spatial_grid();
        }

        let (w, h) = (self.width as f32, self.height as f32);
        let mut pc = PaintCtx::new();

        // The window plate, then the title strip carved one step down into it
        // (the recessed header idiom — its only wall faces the content).
        let mut plate = cce_ui::color::page_low_color();
        if plate[3] > 0.001 {
            plate[3] = cce_ui::color::root_plate_opacity();
        }
        pc.plate_spec(&PlateSpec {
            rect: Rect { x: 0.0, y: 0.0, width: w, height: h },
            color: plate,
            blur: false,
            window_corners: (true, true, true, true),
            depth: cce_ui::layout::bevel_width(),
        });
        pc.recess_edges(
            Rect { x: 0.0, y: 0.0, width: w, height: m.band_h },
            (0.0, 0.0, 0.0, 0.0),
            cce_ui::layout::bar_wall_width(),
            (false, false, true, false),
        );

        let (title_family, _) = cce_ui::layout::menubar_font_parsed();
        pc.text_with(
            "cce-list".to_string(),
            m.pad,
            cce_ui::layout::align_text_y(0.0, m.band_h, TITLE_FONT_SIZE, 0.0),
            TITLE_FONT_SIZE,
            srgb_u8(cce_ui::colors::TEXT_HEADER),
            Some(title_family),
            None,
        );

        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.input_box, &mut pc);

        // The rows, clipped to the viewport so a scrolled list never bleeds
        // into the input or the plate's bottom roll.
        let (family, font_size) = cce_ui::layout::list_font_parsed();
        let vp = self.list_viewport(&m);
        pc.clip(vp, |pc| {
            if self.items.is_empty() {
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
            for (i, item) in self.items.iter().enumerate() {
                let r = self.row_rect(&m, i);
                if r.y + r.height < vp.y || r.y > vp.y + vp.height {
                    continue;
                }
                let hovered = self.hovered_row == Some(i);
                let (cx, cy) = (r.x + CHECK_R, r.y + r.height / 2.0);
                pc.border(
                    Rect {
                        x: cx - CHECK_R,
                        y: cy - CHECK_R,
                        width: 2.0 * CHECK_R,
                        height: 2.0 * CHECK_R,
                    },
                    (CHECK_R, CHECK_R, CHECK_R, CHECK_R),
                    [0.0, 0.0, 0.0, 0.0],
                    cce_ui::colors::TEXT_DIM,
                    1.5,
                );
                if item.done {
                    pc.circle(cx, cy, CHECK_R - 3.0, cce_ui::colors::TOGGLE_ON);
                }
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

    /// Drag the window by its title band; everywhere else is content.
    fn is_movable_backplate_at(&self, _px: f32, py: f32) -> bool {
        py <= metrics(self.width as f32).band_h
    }

    fn clear_color(&self) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        self.pointer = Some((pos.x, pos.y));
        let hovered = self.row_at(pos.x, pos.y);
        if hovered != self.hovered_row {
            self.hovered_row = hovered;
            *needs_rebuild = true;
        }
        let ev = Event::PointerMove { x: pos.x, y: pos.y, local_x: pos.x, local_y: pos.y };
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
        if button == MouseButton::Left && state == ElementState::Pressed {
            if let Some(i) = self.row_at(px, py) {
                let m = metrics(self.width as f32);
                let d = Self::delete_rect(self.row_rect(&m, i));
                if px >= d.x && px <= d.x + d.width && py >= d.y && py <= d.y + d.height {
                    self.items.remove(i);
                    self.clamp_scroll();
                    self.hovered_row = self.row_at(px, py);
                } else {
                    self.items[i].done = !self.items[i].done;
                }
                save_items(&self.items);
                self.needs_rebuild = true;
                *needs_rebuild = true;
                return None;
            }
        }
        if state == ElementState::Pressed && !self.input_box.hit_test(px, py, &self.ui_context) {
            self.input_box.unfocus();
            *needs_rebuild = true;
        }
        let ev = Event::MouseButton { button, state, x: px, y: py, local_x: px, local_y: py };
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
        if self.max_scroll(&m) <= 0.0 {
            return;
        }
        let before = self.scroll;
        self.scroll -= delta.notches_y() * ROW_H;
        self.clamp_scroll();
        if self.scroll != before {
            if let Some((px, py)) = self.pointer {
                self.hovered_row = self.row_at(px, py);
            }
            self.needs_rebuild = true;
            *needs_rebuild = true;
        }
    }

    fn handle_key_input(
        &mut self,
        event: &KeyEvent,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
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
                *needs_rebuild = true;
                return None;
            }
            if let Key::Named(NamedKey::Enter) = event.logical_key {
                // The box's own edit mode, not `focused(&ctx)`: a click focuses
                // through the thread-local focus registry, so the UiContext's
                // focused_widget — which that checks — never learns of it.
                if self.input_box.editing {
                    self.submit_input();
                    *needs_rebuild = true;
                    return None;
                }
            }
        }
        let ev = Event::KeyInput(event.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checklist_round_trips() {
        let items = vec![
            Item { text: "water the plants".into(), done: false },
            Item { text: "renew passport".into(), done: true },
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
                Item { text: "buy stamps".into(), done: false },
                Item { text: "call mom".into(), done: true },
                Item { text: "indented".into(), done: false },
            ]
        );
    }
}
