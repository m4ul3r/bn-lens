//! The marks view: every annotation on the binary — comments (`;`) and
//! tags/bookmarks (`t`), plus BN's own analysis tags — merged into one
//! navigable list. This is the read half of the "shared map": `Enter` jumps to
//! the annotated function so you (and the agent) can move between marked spots.
//! Your own marks (comments, Bookmarks) sort to the top.

use crate::bn::Mark;
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

pub struct MarksList {
    items: Vec<Mark>,
    awidth: usize,
    kwidth: usize,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sel: usize,
    top: usize,
    pending_g: bool,
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok()
}

/// Sort rank: your own annotations first (comments, then Bookmarks), then any
/// other tags (BN analysis tags, custom types).
fn rank(kind: &str) -> u8 {
    match kind {
        "comment" => 0,
        "Bookmarks" | "Important" => 1,
        _ => 2,
    }
}

impl MarksList {
    pub fn new(ctx: &Ctx) -> Self {
        let items = Self::build(ctx);
        let awidth = items
            .iter()
            .map(|it| it.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);
        let kwidth = items
            .iter()
            .map(|it| it.kind.chars().count())
            .max()
            .unwrap_or(7)
            .clamp(7, 24);
        MarksList {
            items,
            awidth,
            kwidth,
            filter: String::new(),
            prev_filter: String::new(),
            mode: Mode::Normal,
            sel: 0,
            top: 0,
            pending_g: false,
        }
    }

    pub fn refresh(&mut self, ctx: &Ctx) {
        self.items = Self::build(ctx);
        self.awidth = self
            .items
            .iter()
            .map(|it| it.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);
        self.kwidth = self
            .items
            .iter()
            .map(|it| it.kind.chars().count())
            .max()
            .unwrap_or(7)
            .clamp(7, 24);
        self.sel = self.sel.min(self.filtered().len().saturating_sub(1));
        self.top = self.top.min(self.sel);
    }

    fn build(ctx: &Ctx) -> Vec<Mark> {
        let mut items = ctx.bn.marks();
        // A function-scoped tag has no address of its own — fill it from the
        // function's entry so it sorts and displays with a real address.
        for m in &mut items {
            if m.addr.is_empty() {
                if let Some(addr) = ctx.addr_by_name.get(&m.func) {
                    m.addr = addr.clone();
                }
            }
        }
        items.sort_by(|a, b| {
            rank(&a.kind).cmp(&rank(&b.kind)).then(
                parse_hex(&a.addr)
                    .unwrap_or(0)
                    .cmp(&parse_hex(&b.addr).unwrap_or(0)),
            )
        });
        items
    }

    pub fn is_searching(&self) -> bool {
        matches!(self.mode, Mode::Search)
    }

    pub fn popup_open(&self) -> bool {
        false
    }

    fn filtered(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                f.is_empty()
                    || it.text.to_lowercase().contains(&f)
                    || it.addr.contains(&f)
                    || it.kind.to_lowercase().contains(&f)
                    || it.func.to_lowercase().contains(&f)
            })
            .collect()
    }

    fn move_sel(&mut self, delta: i64) {
        let len = self.filtered().len() as i64;
        if len == 0 {
            return;
        }
        self.sel = (self.sel as i64 + delta).clamp(0, len - 1) as usize;
    }

    fn selected(&self) -> Option<&Mark> {
        let rows = self.filtered();
        rows.get(self.sel).map(|&i| &self.items[i])
    }

    /// `Enter`: a code mark (has a function) opens that function's decompile; a
    /// data-address mark (no function — e.g. a comment on a global) opens its
    /// xrefs instead, since `bn decompile <data addr>` wouldn't resolve.
    fn open_action(&self) -> Action {
        match self.selected() {
            Some(m) if !m.func.is_empty() => Action::OpenDecompile(m.func.clone()),
            Some(m) if !m.addr.is_empty() => Action::OpenXrefs(m.addr.clone()),
            _ => Action::None,
        }
    }

    /// `x`: cross-reference the mark. A code mark xrefs its containing function
    /// (callers — refs to a single interior instruction are usually empty); a
    /// data mark xrefs the address (who references this global).
    fn xref_action(&self) -> Action {
        match self.selected() {
            Some(m) if !m.func.is_empty() => Action::OpenXrefs(m.func.clone()),
            Some(m) if !m.addr.is_empty() => Action::OpenXrefs(m.addr.clone()),
            _ => Action::None,
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Mode::Search = self.mode {
            match k.code {
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    return self.open_action();
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
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.sel = 0;
                }
                _ => {}
            }
            return Action::None;
        }

        if self.pending_g {
            self.pending_g = false;
            if k.code == KeyCode::Char('g') {
                self.sel = 0;
                return Action::None;
            }
        }
        match k.code {
            // q is the only quit. Esc backs out: clear the filter if set, else
            // return to the Symbols list (never closes the pane).
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
            KeyCode::Char('d') if ctrl => self.move_sel(10),
            KeyCode::Char('u') if ctrl => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(20),
            KeyCode::PageUp => self.move_sel(-20),
            KeyCode::Char('/') => {
                self.prev_filter = self.filter.clone();
                self.filter.clear();
                self.mode = Mode::Search;
                self.sel = 0;
            }
            KeyCode::Enter => return self.open_action(),
            KeyCode::Char('x') => return self.xref_action(),
            _ => {}
        }
        Action::None
    }

    pub fn on_mouse(&mut self, m: MouseEvent, area: Rect) {
        match m.kind {
            MouseEventKind::ScrollUp => self.move_sel(-3),
            MouseEventKind::ScrollDown => self.move_sel(3),
            MouseEventKind::Down(_) => {
                let row = m.row.saturating_sub(area.y + 2) as usize;
                let idx = self.top + row;
                if idx < self.filtered().len() {
                    self.sel = idx;
                }
            }
            _ => {}
        }
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &Ctx) {
        let rows = self.filtered();
        let listh = area.height.saturating_sub(3) as usize;
        if self.sel < self.top {
            self.top = self.sel;
        }
        if listh > 0 && self.sel >= self.top + listh {
            self.top = self.sel + 1 - listh;
        }

        let x0 = area.x;
        let w = area.width as usize;
        let mut bar = crate::ui::crumbs(ctx);
        bar.push(Span::styled(
            format!("   marks  {}/{}", rows.len(), self.items.len()),
            Style::default().add_modifier(Modifier::DIM),
        ));
        crate::ui::render_bar(buf, x0, area.y, w, &bar);

        let (state, keys) = match self.mode {
            Mode::Search => (
                format!(" /{}", self.filter),
                " type · ↑↓ pick · Enter open · Tab list · Esc cancel · ? help",
            ),
            Mode::Normal => (
                if self.filter.is_empty() {
                    " marks · comments + tags".to_string()
                } else {
                    format!(" marks · filter: {}", self.filter)
                },
                " j/k move · / search · Enter open · x xrefs · m menu · i switch · q quit",
            ),
        };
        crate::ui::put_str(
            buf,
            x0,
            area.y + 1,
            state,
            w,
            Style::default().add_modifier(Modifier::DIM),
        );
        crate::ui::render_bar(
            buf,
            x0,
            area.y + area.height.saturating_sub(1),
            w,
            &[Span::styled(
                keys,
                Style::default().add_modifier(Modifier::DIM),
            )],
        );

        if rows.is_empty() {
            crate::ui::put_str(
                buf,
                x0 + 2,
                area.y + 3,
                "no comments or tags yet — add with ; (comment) or t (bookmark) in the viewer",
                w.saturating_sub(4),
                Style::default().add_modifier(Modifier::DIM),
            );
            return;
        }

        for (row, &i) in rows.iter().enumerate().skip(self.top).take(listh) {
            let y = area.y + 2 + (row - self.top) as u16;
            let it = &self.items[i];
            let is_sel = row == self.sel;
            let text = it.text.replace('\n', " ");
            if is_sel {
                let line = format!(
                    "▸ {:<aw$}  {:<kw$}  {}",
                    it.addr,
                    it.kind,
                    text,
                    aw = self.awidth,
                    kw = self.kwidth
                );
                crate::ui::put_str(
                    buf,
                    x0,
                    y,
                    format!("{line:<w$}"),
                    w,
                    Style::default().add_modifier(Modifier::REVERSED),
                );
                continue;
            }
            let kind_style = match it.kind.as_str() {
                "comment" => Style::default().fg(Color::Cyan),
                "Bookmarks" | "Important" => Style::default().fg(Color::Yellow),
                _ => Style::default().fg(Color::DarkGray),
            };
            let spans = vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("{:<aw$}", it.addr, aw = self.awidth),
                    Style::default()
                        .fg(crate::theme::ADDR)
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(format!("  {:<kw$}", it.kind, kw = self.kwidth), kind_style),
                Span::styled(format!("  {text}"), Style::default().fg(crate::theme::NAME)),
            ];
            crate::ui::put_spans(buf, x0, y, w, &spans);
        }
    }
}
