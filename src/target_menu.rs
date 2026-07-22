//! Target dropdown — clicking the `-t <target>` crumb on the header opens a
//! small anchored list of the instance's open targets; picking one re-points
//! the lens at that target without going through the full `i` switcher.

use crate::bn::TargetItem;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

pub enum Outcome {
    Continue,
    Cancel,
    /// Re-point the lens at this `-t` selector (same instance).
    Apply(String),
}

pub struct TargetMenu {
    items: Vec<TargetItem>,
    sel: usize,
    /// Scroll offset (first visible row) when the list outgrows the pane.
    top: usize,
    /// Header column of the `-t` crumb the box anchors under.
    anchor_x: u16,
    /// Screen rows of each drawn entry (for click hit-testing): (y, item-idx).
    hit: Vec<(u16, usize)>,
    /// The drawn box's horizontal span `[x0, x1)`.
    box_x: (u16, u16),
}

impl TargetMenu {
    /// `current` is the lens's own `-t` selector — preferred over bn's `active`
    /// flag for the initial cursor, since the lens pins its target explicitly.
    pub fn new(items: Vec<TargetItem>, current: &str, anchor_x: u16) -> Self {
        let sel = items
            .iter()
            .position(|t| t.selector == current)
            .or_else(|| items.iter().position(|t| t.active))
            .unwrap_or(0);
        TargetMenu {
            items,
            sel,
            top: 0,
            anchor_x,
            hit: Vec::new(),
            box_x: (0, 0),
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Outcome {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Cancel,
            KeyCode::Char('j') | KeyCode::Down => {
                self.sel = (self.sel + 1).min(self.items.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(t) = self.items.get(self.sel) {
                    return Outcome::Apply(t.selector.clone());
                }
                return Outcome::Cancel;
            }
            _ => {}
        }
        Outcome::Continue
    }

    /// A click on an entry selects it; a click anywhere else dismisses. The
    /// wheel moves the cursor (target lists can outgrow the box).
    pub fn on_mouse(&mut self, m: MouseEvent) -> Outcome {
        match m.kind {
            MouseEventKind::ScrollDown => {
                self.sel = (self.sel + 1).min(self.items.len().saturating_sub(1));
                Outcome::Continue
            }
            MouseEventKind::ScrollUp => {
                self.sel = self.sel.saturating_sub(1);
                Outcome::Continue
            }
            MouseEventKind::Down(_) => {
                let (x0, x1) = self.box_x;
                if m.column >= x0 && m.column < x1 {
                    if let Some(&(_, idx)) = self.hit.iter().find(|(y, _)| *y == m.row) {
                        return Outcome::Apply(self.items[idx].selector.clone());
                    }
                }
                Outcome::Cancel // click-away dismiss
            }
            _ => Outcome::Continue,
        }
    }

    fn label(t: &TargetItem) -> String {
        let analysis = match t.analysis_state.as_str() {
            "full" => "",
            "quick" => "[Q] ",
            _ => "[?] ",
        };
        format!(
            "{}{}{}",
            if t.active { "● " } else { "  " },
            analysis,
            ui::clean_target_label(&t.selector)
        )
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.hit.clear();
        let labels: Vec<String> = self.items.iter().map(Self::label).collect();
        let width = (labels.iter().map(|l| l.chars().count()).max().unwrap_or(20) as u16 + 4)
            .min(area.width.saturating_sub(2))
            .max(12);
        // Anchor under the `-t` crumb, pulled left if the box would spill off.
        let x = self
            .anchor_x
            .min(area.x + area.width.saturating_sub(width + 1));
        let y = area.y + 1;
        let view_h = (self.items.len())
            .min(area.height.saturating_sub(4) as usize)
            .max(1);
        let height = view_h as u16 + 2;
        self.box_x = (x, x + width);
        // keep the cursor visible
        if self.sel < self.top {
            self.top = self.sel;
        }
        if self.sel >= self.top + view_h {
            self.top = self.sel + 1 - view_h;
        }
        ui::draw_box(buf, x, y, width, height, "targets");
        for (row, idx) in (self.top..self.items.len()).take(view_h).enumerate() {
            let yy = y + 1 + row as u16;
            let selected = idx == self.sel;
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if self.items[idx].active {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            crate::ui::put_str(
                buf,
                x + 1,
                yy,
                format!(
                    " {:<w$}",
                    labels[idx],
                    w = (width as usize).saturating_sub(3)
                ),
                (width as usize).saturating_sub(2),
                style,
            );
            self.hit.push((yy, idx));
        }
    }
}
