//! The `bn lens` dropdown. Clicking the title (or `m`) opens a small anchored
//! menu — the discoverable home for switching the top-level view and reaching
//! the global actions that are otherwise keyboard-only.

use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// The title button occupies `" bn lens "` at the top-left (columns 0..TITLE_W).
pub const TITLE_W: u16 = 9;

#[derive(Clone, Copy, PartialEq)]
pub enum Choice {
    Symbols,
    Strings,
    Imports,
    Exports,
    Marks,
    Refresh,
    SwitchBn,
    Help,
    Quit,
}

impl Choice {
    fn label(self) -> &'static str {
        match self {
            Choice::Symbols => "Symbols   functions + data",
            Choice::Strings => "Strings   recovered text · xref uses",
            Choice::Imports => "Imports   attack surface · sinks",
            Choice::Exports => "Exports   public API · functions + data",
            Choice::Marks => "Marks     comments · tags · bookmarks",
            Choice::Refresh => "Refresh   re-sync the live bn instance",
            Choice::SwitchBn => "Switch bn instance / target…",
            Choice::Help => "Help      shortcut guide",
            Choice::Quit => "Quit",
        }
    }
}

const ITEMS: &[Choice] = &[
    Choice::Symbols,
    Choice::Strings,
    Choice::Imports,
    Choice::Exports,
    Choice::Marks,
    Choice::Refresh,
    Choice::SwitchBn,
    Choice::Help,
    Choice::Quit,
];

#[derive(Default)]
pub struct Menu {
    open: bool,
    sel: usize,
    /// Screen rows of each drawn entry (for click hit-testing): (y, choice-idx).
    hit: Vec<(u16, usize)>,
    /// The drawn box's horizontal span `[x0, x1)`, so a click must land inside
    /// the box columns — not just anywhere on an entry's row.
    box_x: (u16, u16),
}

impl Menu {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, active: Choice) {
        self.open = true;
        self.sel = ITEMS.iter().position(|&c| c == active).unwrap_or(0);
    }

    pub fn toggle(&mut self, active: Choice) {
        if self.open {
            self.open = false;
        } else {
            self.open(active);
        }
    }

    /// Did a click land on the title button?
    pub fn hit_title(area: Rect, m: MouseEvent) -> bool {
        matches!(m.kind, MouseEventKind::Down(_))
            && m.row == area.y
            && m.column >= area.x
            && m.column < area.x + TITLE_W
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Option<Choice> {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('m') => self.open = false,
            KeyCode::Char('j') | KeyCode::Down => {
                self.sel = (self.sel + 1).min(ITEMS.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Enter => {
                self.open = false;
                return Some(ITEMS[self.sel]);
            }
            _ => {}
        }
        None
    }

    /// A click inside the box selects the entry on that row; a click anywhere
    /// else dismisses the menu.
    pub fn on_mouse(&mut self, m: MouseEvent) -> Option<Choice> {
        if !matches!(m.kind, MouseEventKind::Down(_)) {
            return None;
        }
        let (x0, x1) = self.box_x;
        let in_box = m.column >= x0 && m.column < x1;
        if in_box {
            if let Some(&(_, idx)) = self.hit.iter().find(|(y, _)| *y == m.row) {
                self.open = false;
                return Some(ITEMS[idx]);
            }
        }
        self.open = false; // click-away dismiss
        None
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, active: Choice) {
        self.hit.clear();
        if !self.open {
            return;
        }
        let width = ITEMS
            .iter()
            .map(|c| c.label().chars().count())
            .max()
            .unwrap_or(20) as u16
            + 6;
        let width = width.min(area.width.saturating_sub(2));
        let height = ITEMS.len() as u16 + 2;
        let x = area.x;
        let y = area.y + 1;
        self.box_x = (x, x + width);
        ui::draw_box(buf, x, y, width, height, "view");
        for (row, choice) in ITEMS.iter().enumerate() {
            let yy = y + 1 + row as u16;
            let selected = row == self.sel;
            let is_active = *choice == active;
            let marker = if is_active { "●" } else { " " };
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if is_active {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            crate::ui::put_str(
                buf,
                x + 1,
                yy,
                format!(
                    " {marker} {:<w$}",
                    choice.label(),
                    w = (width as usize).saturating_sub(5)
                ),
                (width as usize).saturating_sub(2),
                style,
            );
            self.hit.push((yy, row));
        }
    }
}
