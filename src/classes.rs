//! C++ class-lens view. This is intentionally a compact front-end to
//! `bn class list/show`: domain classes (STL/vendor folded) lead, and Enter/p
//! opens the backend's RTTI/vtable/base/method/construction evidence.

use crate::bn::ClassItem;
use crate::ctx::Ctx;
use crate::picker::Action;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

enum Mode {
    Normal,
    Search,
}

struct Evidence {
    title: String,
    lines: Vec<String>,
    off: usize,
    hoff: usize,
}

pub struct ClassesList {
    items: Vec<ClassItem>,
    error: Option<String>,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sel: usize,
    top: usize,
    pending_g: bool,
    evidence: Option<Evidence>,
}

impl ClassesList {
    pub fn new(ctx: &Ctx) -> Self {
        let (items, error) = match ctx.bn.classes_list() {
            Ok(items) => (items, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        Self {
            items,
            error,
            filter: String::new(),
            prev_filter: String::new(),
            mode: Mode::Normal,
            sel: 0,
            top: 0,
            pending_g: false,
            evidence: None,
        }
    }

    pub fn refresh(&mut self, ctx: &Ctx) {
        match ctx.bn.classes_list() {
            Ok(items) => {
                self.items = items;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        self.sel = self.sel.min(self.filtered().len().saturating_sub(1));
        self.top = self.top.min(self.sel);
        self.evidence = None;
    }

    pub fn is_searching(&self) -> bool {
        matches!(self.mode, Mode::Search)
    }

    pub fn popup_open(&self) -> bool {
        self.evidence.is_some()
    }

    fn filtered(&self) -> Vec<usize> {
        let filter = self.filter.to_ascii_lowercase();
        (0..self.items.len())
            .filter(|&index| {
                let item = &self.items[index];
                filter.is_empty()
                    || item.name.to_ascii_lowercase().contains(&filter)
                    || item.confidence.to_ascii_lowercase().contains(&filter)
                    || item
                        .bases
                        .iter()
                        .any(|base| base.to_ascii_lowercase().contains(&filter))
            })
            .collect()
    }

    fn current(&self) -> Option<&ClassItem> {
        let filtered = self.filtered();
        filtered.get(self.sel).map(|&index| &self.items[index])
    }

    fn move_sel(&mut self, delta: i64) {
        let len = self.filtered().len() as i64;
        if len > 0 {
            self.sel = (self.sel as i64 + delta).clamp(0, len - 1) as usize;
        }
    }

    fn open_evidence(&mut self, ctx: &Ctx) {
        let Some(name) = self.current().map(|item| item.name.clone()) else {
            return;
        };
        self.evidence = Some(Evidence {
            title: format!("class · {name}"),
            lines: ctx.bn.class_show(&name),
            off: 0,
            hoff: 0,
        });
    }

    fn evidence_key(&mut self, key: KeyEvent) -> Action {
        let Some(evidence) = &mut self.evidence else {
            return Action::None;
        };
        let count = evidence.lines.len();
        let max_h = evidence
            .lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter | KeyCode::Char('p') => {
                self.evidence = None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                evidence.off = (evidence.off + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => evidence.off = evidence.off.saturating_sub(1),
            KeyCode::PageDown => evidence.off = (evidence.off + 10).min(count.saturating_sub(1)),
            KeyCode::PageUp => evidence.off = evidence.off.saturating_sub(10),
            KeyCode::Char('h') | KeyCode::Left => evidence.hoff = evidence.hoff.saturating_sub(4),
            KeyCode::Char('l') | KeyCode::Right => evidence.hoff = (evidence.hoff + 4).min(max_h),
            KeyCode::Char('0') => evidence.hoff = 0,
            _ => {}
        }
        Action::None
    }

    pub fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if self.evidence.is_some() {
            return self.evidence_key(key);
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Mode::Search = self.mode {
            match key.code {
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    self.open_evidence(ctx);
                }
                KeyCode::Tab => self.mode = Mode::Normal,
                KeyCode::Esc => {
                    self.filter = self.prev_filter.clone();
                    self.mode = Mode::Normal;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.sel = 0;
                }
                KeyCode::Down => self.move_sel(1),
                KeyCode::Up => self.move_sel(-1),
                KeyCode::Char(ch) => {
                    self.filter.push(ch);
                    self.sel = 0;
                }
                _ => {}
            }
            return Action::None;
        }

        if self.pending_g {
            self.pending_g = false;
            if key.code == KeyCode::Char('g') {
                self.sel = 0;
                return Action::None;
            }
        }
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    return Action::Home;
                }
                self.filter.clear();
                self.sel = 0;
                self.top = 0;
            }
            KeyCode::Char('i') => return Action::Switch,
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('G') => self.move_sel(i64::MAX / 2),
            KeyCode::Char('d') if control => self.move_sel(10),
            KeyCode::Char('u') if control => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(20),
            KeyCode::PageUp => self.move_sel(-20),
            KeyCode::Char('/') => {
                self.prev_filter = self.filter.clone();
                self.filter.clear();
                self.mode = Mode::Search;
                self.sel = 0;
            }
            KeyCode::Enter | KeyCode::Char('p') => self.open_evidence(ctx),
            _ => {}
        }
        Action::None
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent, area: Rect) {
        if let Some(evidence) = &mut self.evidence {
            let count = evidence.lines.len();
            match mouse.kind {
                MouseEventKind::ScrollUp => evidence.off = evidence.off.saturating_sub(3),
                MouseEventKind::ScrollDown => {
                    evidence.off = (evidence.off + 3).min(count.saturating_sub(1))
                }
                MouseEventKind::Down(_) => self.evidence = None,
                _ => {}
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_sel(-20),
            MouseEventKind::ScrollDown => self.move_sel(20),
            MouseEventKind::Down(_) => {
                let row = mouse.row.saturating_sub(area.y + 2) as usize;
                let index = self.top + row;
                if index < self.filtered().len() {
                    self.sel = index;
                }
            }
            _ => {}
        }
    }

    pub fn render(&mut self, area: Rect, buffer: &mut Buffer, ctx: &Ctx) {
        let rows = self.filtered();
        let list_height = area.height.saturating_sub(3) as usize;
        if self.sel < self.top {
            self.top = self.sel;
        }
        if list_height > 0 && self.sel >= self.top + list_height {
            self.top = self.sel + 1 - list_height;
        }

        let width = area.width as usize;
        let vtables = self.items.iter().filter(|item| item.has_vtable).count();
        let mut bar = crate::ui::crumbs(ctx);
        bar.push(crate::ui::crumb_sep());
        bar.push(Span::styled(
            format!(
                "classes  {}/{}  · {} vtable · STL/vendor folded",
                rows.len(),
                self.items.len(),
                vtables
            ),
            Style::default().add_modifier(Modifier::DIM),
        ));
        crate::ui::render_bar(buffer, area.x, area.y, width, &bar);

        let state = match self.mode {
            Mode::Search => format!(" /{}", self.filter),
            Mode::Normal if !self.filter.is_empty() => format!(" filter: {}", self.filter),
            Mode::Normal => String::new(),
        };
        crate::ui::put_str(
            buffer,
            area.x,
            area.y + 1,
            state,
            width,
            Style::default().add_modifier(Modifier::DIM),
        );
        crate::ui::render_bar(
            buffer,
            area.x,
            area.y + area.height.saturating_sub(1),
            width,
            &crate::ui::hint_bar(&[
                &[("j/k", "move"), ("/", "search")],
                &[("Enter/p", "evidence")],
                &[("m", "menu"), ("v", "list"), ("i", "switch")],
                &[("q", "quit")],
            ]),
        );

        if let Some(error) = &self.error {
            crate::ui::put_str(
                buffer,
                area.x + 2,
                area.y + 3,
                format!("✗ {error}"),
                width.saturating_sub(4),
                Style::default().fg(Color::Red),
            );
        } else if rows.is_empty() {
            let message = if ctx.analysis_complete() {
                "no domain C++ classes recovered"
            } else {
                "no classes observed in incomplete analysis"
            };
            crate::ui::put_str(
                buffer,
                area.x + 2,
                area.y + 3,
                message,
                width.saturating_sub(4),
                Style::default().add_modifier(Modifier::DIM),
            );
        }

        if self.error.is_none() {
            for (row, &index) in rows.iter().enumerate().skip(self.top).take(list_height) {
                let y = area.y + 2 + (row - self.top) as u16;
                let item = &self.items[index];
                let bases = if item.bases.is_empty() {
                    String::new()
                } else {
                    format!(" : {}", item.bases.join(", "))
                };
                let summary = format!(
                    "  {}  methods={}  {}  [{}]{}",
                    item.name,
                    item.method_count,
                    if item.has_vtable {
                        "vtable"
                    } else {
                        "no-vtable"
                    },
                    item.confidence,
                    bases
                );
                let style = if row == self.sel {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else if item.has_vtable {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(crate::theme::NAME)
                };
                crate::ui::put_str(
                    buffer,
                    area.x,
                    y,
                    format!("{summary:<width$}"),
                    width,
                    style,
                );
            }
        }

        self.render_evidence(area, buffer);
    }

    fn render_evidence(&self, area: Rect, buffer: &mut Buffer) {
        let Some(evidence) = &self.evidence else {
            return;
        };
        let box_width = area.width.saturating_sub(6).clamp(50, 110);
        let box_height = area.height.saturating_sub(4).clamp(8, 30);
        let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
        let box_y = area.y + (area.height.saturating_sub(box_height)) / 2;
        crate::ui::draw_box(buffer, box_x, box_y, box_width, box_height, &evidence.title);
        let view_height = box_height.saturating_sub(3) as usize;
        let inner = box_width.saturating_sub(4) as usize;
        for (row, line) in evidence
            .lines
            .iter()
            .skip(evidence.off)
            .take(view_height)
            .enumerate()
        {
            let visible: String = line.chars().skip(evidence.hoff).collect();
            crate::ui::put_str(
                buffer,
                box_x + 2,
                box_y + 1 + row as u16,
                visible,
                inner,
                Style::default().fg(Color::Yellow),
            );
        }
        crate::ui::put_str(
            buffer,
            box_x + 2,
            box_y + box_height - 1,
            " j/k scroll · h/l pan · 0 reset · Enter/p/q/Esc close ",
            inner,
            Style::default().add_modifier(Modifier::DIM),
        );
    }
}
