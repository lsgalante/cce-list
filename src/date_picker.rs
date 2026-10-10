//! The due-date picker: a month grid drawn by the app over the list (cce-ui
//! has no date widget). A row's calendar glyph or its date label opens it;
//! a day, Today, Tomorrow or Clear sets the item's date and closes it.
//!
//! Keys while open: arrows move the day (crossing months as they go),
//! PageUp/PageDown the month, Home today, Enter picks, Delete clears,
//! Escape closes. The wheel over it flips months.

use chrono::{Datelike, Days, Months, NaiveDate};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{AlignH, AlignV, PaintCtx, TextAttrs, TextLayout};
use cce_ui::widget::{Key, KeyEvent, NamedKey};

const CELL: f32 = 25.0;
/// Inset of the content from the plate's edge: clear of its carved rim.
const PAD: f32 = 12.0;
/// The item's name, above the month: the picker can cover its own row.
const TITLE_H: f32 = 20.0;
const HEAD_H: f32 = 28.0;
const WEEK_H: f32 = 18.0;
const FOOT_H: f32 = 28.0;
pub const WIDTH: f32 = 7.0 * CELL + 2.0 * PAD;
pub const HEIGHT: f32 = PAD + TITLE_H + HEAD_H + WEEK_H + 6.0 * CELL + 4.0 + FOOT_H + PAD;

const WEEKDAYS: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
/// The chosen day's fill and today's ring — the calendar app's accent.
const ACCENT: [f32; 4] = [0.22, 0.42, 0.85, 1.0];
const WASH: [f32; 4] = [1.0, 1.0, 1.0, 0.10];
const TEXT: [u8; 3] = [225, 228, 232];
const TEXT_DIM: [u8; 3] = [140, 145, 152];
const TEXT_FAINT: [u8; 3] = [95, 100, 108];

/// What the pointer is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Prev,
    Next,
    Day(NaiveDate),
    Today,
    Tomorrow,
    Clear,
    /// Inside the picker but on nothing that acts.
    Inside,
}

/// What the picker asks of the item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Set(NaiveDate),
    Clear,
    Close,
    /// The picker changed (month, cursor); redraw.
    Redraw,
    /// Not the picker's.
    Ignored,
}

pub struct DatePicker {
    /// The list row it sets the date of, and that item's text.
    pub row: usize,
    title: String,
    pub rect: Rect,
    /// The first of the month shown.
    month: NaiveDate,
    /// The item's date when the picker opened.
    current: Option<NaiveDate>,
    /// The keyboard's day: arrows move it, Enter picks it.
    cursor: NaiveDate,
    pub hover: Option<Hit>,
}

fn first_of(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap_or(d)
}

impl DatePicker {
    /// Open under `anchor` (the row), right-aligned to `right`, flipped
    /// above it when there is no room below, and kept inside the window.
    pub fn open(row: usize, title: String, anchor: Rect, right: f32, current: Option<NaiveDate>, today: NaiveDate, win: (f32, f32)) -> Self {
        let x = (right - WIDTH).clamp(PAD.min(win.0 - WIDTH), (win.0 - WIDTH - PAD).max(0.0));
        let below = anchor.y + anchor.height;
        let y = if below + HEIGHT <= win.1 || anchor.y - HEIGHT < 0.0 { below } else { anchor.y - HEIGHT };
        let y = y.clamp(0.0, (win.1 - HEIGHT).max(0.0));
        let cursor = current.unwrap_or(today);
        DatePicker {
            row,
            title,
            rect: Rect { x, y, width: WIDTH, height: HEIGHT },
            month: first_of(cursor),
            current,
            cursor,
            hover: None,
        }
    }

    // ── geometry ──────────────────────────────────────────────────────────

    fn head(&self) -> Rect {
        Rect { x: self.rect.x + PAD, y: self.rect.y + PAD + TITLE_H, width: 7.0 * CELL, height: HEAD_H }
    }

    fn prev_rect(&self) -> Rect {
        let h = self.head();
        Rect { x: h.x, y: h.y, width: CELL, height: HEAD_H }
    }

    fn next_rect(&self) -> Rect {
        let h = self.head();
        Rect { x: h.x + h.width - CELL, y: h.y, width: CELL, height: HEAD_H }
    }

    fn grid_top(&self) -> f32 {
        self.rect.y + PAD + TITLE_H + HEAD_H + WEEK_H
    }

    /// The grid's first cell: the Monday on or before the 1st.
    fn first_cell(&self) -> NaiveDate {
        let back = self.month.weekday().num_days_from_monday() as u64;
        self.month.checked_sub_days(Days::new(back)).unwrap_or(self.month)
    }

    fn day_rect(&self, i: usize) -> Rect {
        Rect {
            x: self.rect.x + PAD + (i % 7) as f32 * CELL,
            y: self.grid_top() + (i / 7) as f32 * CELL,
            width: CELL,
            height: CELL,
        }
    }

    /// The footer's three buttons: Today, Tomorrow, Clear.
    fn foot_rects(&self) -> [Rect; 3] {
        let y = self.grid_top() + 6.0 * CELL + 4.0;
        let w = 7.0 * CELL / 3.0;
        let x0 = self.rect.x + PAD;
        std::array::from_fn(|i| Rect { x: x0 + i as f32 * w, y, width: w, height: FOOT_H })
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.rect.contains(x, y)
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        if !self.contains(x, y) {
            return None;
        }
        if self.prev_rect().contains(x, y) {
            return Some(Hit::Prev);
        }
        if self.next_rect().contains(x, y) {
            return Some(Hit::Next);
        }
        let first = self.first_cell();
        if let Some(i) = (0..42).find(|&i| self.day_rect(i).contains(x, y)) {
            return first.checked_add_days(Days::new(i as u64)).map(Hit::Day).or(Some(Hit::Inside));
        }
        let [today, tomorrow, clear] = self.foot_rects();
        Some(if today.contains(x, y) {
            Hit::Today
        } else if tomorrow.contains(x, y) {
            Hit::Tomorrow
        } else if clear.contains(x, y) {
            Hit::Clear
        } else {
            Hit::Inside
        })
    }

    // ── input ─────────────────────────────────────────────────────────────

    pub fn shift_month(&mut self, delta: i32) {
        let m = Months::new(delta.unsigned_abs());
        let moved = if delta < 0 { self.month.checked_sub_months(m) } else { self.month.checked_add_months(m) };
        if let Some(month) = moved {
            self.month = month;
        }
    }

    /// A press on `hit`.
    pub fn press(&mut self, hit: Hit, today: NaiveDate) -> Outcome {
        match hit {
            Hit::Prev => {
                self.shift_month(-1);
                Outcome::Redraw
            }
            Hit::Next => {
                self.shift_month(1);
                Outcome::Redraw
            }
            Hit::Day(d) => Outcome::Set(d),
            Hit::Today => Outcome::Set(today),
            Hit::Tomorrow => today.succ_opt().map_or(Outcome::Ignored, Outcome::Set),
            Hit::Clear => Outcome::Clear,
            Hit::Inside => Outcome::Ignored,
        }
    }

    fn move_cursor(&mut self, days: i64) {
        let moved = if days < 0 {
            self.cursor.checked_sub_days(Days::new(days.unsigned_abs()))
        } else {
            self.cursor.checked_add_days(Days::new(days as u64))
        };
        if let Some(c) = moved {
            self.cursor = c;
            self.month = first_of(c);
        }
    }

    pub fn key(&mut self, event: &KeyEvent, today: NaiveDate) -> Outcome {
        let Key::Named(key) = &event.logical_key else { return Outcome::Ignored };
        match key {
            NamedKey::Escape => Outcome::Close,
            NamedKey::Enter => Outcome::Set(self.cursor),
            NamedKey::Delete | NamedKey::Backspace => Outcome::Clear,
            NamedKey::ArrowLeft => {
                self.move_cursor(-1);
                Outcome::Redraw
            }
            NamedKey::ArrowRight => {
                self.move_cursor(1);
                Outcome::Redraw
            }
            NamedKey::ArrowUp => {
                self.move_cursor(-7);
                Outcome::Redraw
            }
            NamedKey::ArrowDown => {
                self.move_cursor(7);
                Outcome::Redraw
            }
            NamedKey::PageUp | NamedKey::PageDown => {
                let delta = if *key == NamedKey::PageUp { -1 } else { 1 };
                let m = Months::new(1);
                let moved = if delta < 0 { self.cursor.checked_sub_months(m) } else { self.cursor.checked_add_months(m) };
                if let Some(c) = moved {
                    self.cursor = c;
                    self.month = first_of(c);
                }
                Outcome::Redraw
            }
            NamedKey::Home => {
                self.cursor = today;
                self.month = first_of(today);
                Outcome::Redraw
            }
            _ => Outcome::Ignored,
        }
    }

    // ── paint ─────────────────────────────────────────────────────────────

    fn centred(rect: Rect) -> TextLayout {
        TextLayout { wrap_width: Some(rect.width), box_height: rect.height, align_h: AlignH::Center, align_v: AlignV::Middle }
    }

    fn label(pc: &mut PaintCtx, text: &str, rect: Rect, size: f32, color: [u8; 3], bold: bool) {
        let attrs = TextAttrs { weight: bold.then_some(600), ..Default::default() };
        pc.text_boxed(text.to_string(), rect.x, rect.y, size, color, None, None, attrs, Self::centred(rect));
    }

    /// The surface the toolkit's menus wear — the dropdown's frosted
    /// popover material on a carved plate — then the month, the grid and
    /// the footer.
    pub fn paint(&self, pc: &mut PaintCtx, today: NaiveDate) {
        let radius = cce_ui::layout::dropdown_corner_radius();
        let raw_bg = cce_ui::colors::dropdown_background_color();
        let base = if raw_bg[3] > 0.001 { raw_bg } else { cce_ui::color::page_low_color() };
        let face = cce_ui::scene::Material::popover(base);
        let depth = cce_ui::layout::bevel_width().min(self.rect.height * 0.2);
        let (t, tr) = cce_ui::layout::carve_inside(self.rect, (radius, radius, radius, radius), depth);
        pc.inset_plate(t, tr, Some(&face), depth);

        let hover = |h: Hit| self.hover == Some(h);
        let wash = |pc: &mut PaintCtx, r: Rect| pc.rounded_rect(r, 6.0, (true, true, true, true), WASH);

        // The item, clipped to one line.
        let name = Rect { x: self.rect.x + PAD + 4.0, y: self.rect.y + PAD, width: 7.0 * CELL - 8.0, height: TITLE_H };
        pc.text_with(self.title.clone(), name.x, name.y + 2.0, 11.5, TEXT_DIM, None,
            Some([name.x, name.y, name.x + name.width, name.y + name.height]));

        // Header: ‹ Month Year ›.
        for (hit, rect, icon) in [(Hit::Prev, self.prev_rect(), "chevron-left"), (Hit::Next, self.next_rect(), "chevron-right")] {
            if hover(hit) {
                wash(pc, rect);
            }
            let s = 14.0;
            let glyph = Rect { x: rect.x + (rect.width - s) / 2.0, y: rect.y + (rect.height - s) / 2.0, width: s, height: s };
            pc.icon(icon, glyph, cce_ui::colors::to_srgb(cce_ui::colors::TEXT_FG));
        }
        let h = self.head();
        let title = Rect { x: h.x + CELL, y: h.y, width: h.width - 2.0 * CELL, height: HEAD_H };
        Self::label(pc, &self.month.format("%B %Y").to_string(), title, 13.0, TEXT, true);

        for (i, name) in WEEKDAYS.iter().enumerate() {
            let r = Rect { x: self.rect.x + PAD + i as f32 * CELL, y: self.rect.y + PAD + TITLE_H + HEAD_H, width: CELL, height: WEEK_H };
            Self::label(pc, name, r, 10.0, TEXT_FAINT, false);
        }

        let first = self.first_cell();
        for i in 0..42 {
            let Some(d) = first.checked_add_days(Days::new(i as u64)) else { continue };
            let r = self.day_rect(i);
            let dot = Rect { x: r.x + 2.0, y: r.y + 2.0, width: r.width - 4.0, height: r.height - 4.0 };
            let chosen = self.current == Some(d);
            if chosen {
                pc.rounded_rect(dot, dot.height / 2.0, (true, true, true, true), ACCENT);
            } else if hover(Hit::Day(d)) || self.cursor == d {
                pc.rounded_rect(dot, dot.height / 2.0, (true, true, true, true), WASH);
            }
            if d == today && !chosen {
                let rr = dot.height / 2.0;
                pc.border(dot, (rr, rr, rr, rr), [0.0; 4], ACCENT, 1.5);
            }
            let color = if chosen {
                [255, 255, 255]
            } else if d.month() == self.month.month() {
                TEXT
            } else {
                TEXT_FAINT
            };
            Self::label(pc, &d.day().to_string(), r, 11.5, color, d == today);
        }

        let [r_today, r_tomorrow, r_clear] = self.foot_rects();
        for (hit, rect, text) in [(Hit::Today, r_today, "Today"), (Hit::Tomorrow, r_tomorrow, "Tomorrow"), (Hit::Clear, r_clear, "Clear")] {
            if hover(hit) {
                wash(pc, rect);
            }
            let color = if hit == Hit::Clear && self.current.is_none() { TEXT_FAINT } else { TEXT_DIM };
            Self::label(pc, text, rect, 11.5, color, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn grid_starts_on_the_monday_and_hits_map_to_days() {
        let today = d(2026, 10, 10);
        let p = DatePicker::open(0, String::new(), Rect { x: 0.0, y: 0.0, width: 300.0, height: 26.0 }, 300.0, None, today, (300.0, 600.0));
        // October 2026 starts on a Thursday: the grid opens on Mon Sep 28.
        assert_eq!(p.first_cell(), d(2026, 9, 28));
        let r = p.day_rect(3);
        assert_eq!(p.hit(r.x + 5.0, r.y + 5.0), Some(Hit::Day(d(2026, 10, 1))));
        let [_, tomorrow, _] = p.foot_rects();
        let mut p = p;
        assert_eq!(p.press(p.hit(tomorrow.x + 2.0, tomorrow.y + 2.0).unwrap(), today), Outcome::Set(d(2026, 10, 11)));
        assert_eq!(p.hit(-5.0, -5.0), None);
    }

    #[test]
    fn placement_flips_above_and_stays_in_the_window() {
        let today = d(2026, 10, 10);
        let row = Rect { x: 8.0, y: 280.0, width: 284.0, height: 26.0 };
        let p = DatePicker::open(0, String::new(), row, 290.0, None, today, (300.0, 320.0));
        assert!(p.rect.y + p.rect.height <= 320.0 && p.rect.y >= 0.0);
        assert!(p.rect.x >= 0.0 && p.rect.x + p.rect.width <= 300.0);
    }

    #[test]
    fn keys_walk_days_across_months() {
        let today = d(2026, 10, 10);
        let mut p = DatePicker::open(0, String::new(), Rect { x: 0.0, y: 0.0, width: 300.0, height: 26.0 }, 300.0, Some(d(2026, 10, 31)), today, (300.0, 600.0));
        p.move_cursor(1);
        assert_eq!((p.cursor, p.month), (d(2026, 11, 1), d(2026, 11, 1)));
        p.move_cursor(-7);
        assert_eq!(p.cursor, d(2026, 10, 25));
    }
}
