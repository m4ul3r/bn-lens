//! The exports view: the binary's exported symbols — its public API surface.
//! Functions and data globals are shown distinctly; `Enter` opens a function
//! export's decompile (a data export's xrefs), `x` cross-references either, and
//! `p` peeks *who uses it* (pseudo-C at each callsite). Mirrors the imports view.

use crate::ctx::Ctx;
use crate::picker::Action;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

struct ExpItem {
    addr: String,
    /// Stable identifier for backend actions.
    name: String,
    /// Human-facing demangled/short name.
    display_name: String,
    /// True for a data global; false for a function (drives the action + colour).
    is_data: bool,
}

enum Mode {
    Normal,
    Search,
}

struct Usage {
    title: String,
    addr: String,
    lines: Vec<String>,
    off: usize,
}

pub struct ExportsList {
    items: Vec<ExpItem>,
    awidth: usize,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sel: usize,
    top: usize,
    pending_g: bool,
    usage: Option<Usage>,
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok()
}

impl ExportsList {
    pub fn new(ctx: &Ctx) -> Self {
        let items = Self::build(ctx);
        let awidth = items
            .iter()
            .map(|it| it.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);
        ExportsList {
            items,
            awidth,
            filter: String::new(),
            prev_filter: String::new(),
            mode: Mode::Normal,
            sel: 0,
            top: 0,
            pending_g: false,
            usage: None,
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
        // Keep the cursor valid if the rebuilt list shrank.
        self.sel = self.sel.min(self.filtered().len().saturating_sub(1));
        self.top = self.top.min(self.sel);
    }

    /// Functions first, then data — both by address — so the callable public API
    /// leads and exported globals trail.
    fn build(ctx: &Ctx) -> Vec<ExpItem> {
        let mut items: Vec<ExpItem> = ctx
            .bn
            .exports_list()
            .into_iter()
            .map(|e| {
                // Cross-check bn's `(data)` tag against the known function set:
                // an export that isn't a recovered function is data.
                let is_data = e.is_data
                    || (!ctx.func_names.contains(&e.name)
                        && !ctx.func_names.contains(&e.display_name));
                ExpItem {
                    addr: e.addr,
                    name: e.name,
                    display_name: e.display_name,
                    is_data,
                }
            })
            .collect();
        items.sort_by(|a, b| {
            a.is_data.cmp(&b.is_data).then(
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
        self.usage.is_some()
    }

    fn data_count(&self) -> usize {
        self.items.iter().filter(|it| it.is_data).count()
    }

    fn filtered(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                f.is_empty()
                    || it.name.to_lowercase().contains(&f)
                    || it.display_name.to_lowercase().contains(&f)
                    || it.addr.contains(&f)
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

    fn current(&self) -> Option<&ExpItem> {
        let rows = self.filtered();
        rows.get(self.sel).map(|&i| &self.items[i])
    }

    /// `Enter`: a function export opens its decompile; a data export opens its
    /// xrefs (`bn decompile <data addr>` wouldn't resolve).
    fn open_action(&self) -> Action {
        match self.current() {
            Some(it) if !it.is_data => Action::OpenDecompile(it.name.clone()),
            Some(it) => Action::OpenXrefs(it.addr.clone()),
            None => Action::None,
        }
    }

    /// `x`: cross-reference either kind (functions xref by name, data by address).
    fn xref_action(&self) -> Action {
        match self.current() {
            Some(it) if !it.is_data => Action::OpenXrefs(it.name.clone()),
            Some(it) => Action::OpenXrefs(it.addr.clone()),
            None => Action::None,
        }
    }

    /// `p`: peek where the export is used — exact asm plus approximate C.
    fn open_usage(&mut self, ctx: &Ctx) {
        let Some(item) = self.current() else { return };
        let (addr, name) = (item.addr.clone(), item.display_name.clone());
        self.usage = Some(Usage {
            title: format!("uses of {name}"),
            lines: crate::usage::report(ctx, &addr, &name),
            addr,
            off: 0,
        });
    }

    fn usage_key(&mut self, k: KeyEvent) -> Action {
        let Some(usage) = &mut self.usage else {
            return Action::None;
        };
        let n = usage.lines.len();
        match k.code {
            KeyCode::Enter | KeyCode::Char('x') => {
                let addr = usage.addr.clone();
                self.usage = None;
                return Action::OpenXrefs(addr);
            }
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('p') => self.usage = None,
            KeyCode::Char('j') | KeyCode::Down => {
                usage.off = (usage.off + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => usage.off = usage.off.saturating_sub(1),
            KeyCode::PageDown => usage.off = (usage.off + 10).min(n.saturating_sub(1)),
            KeyCode::PageUp => usage.off = usage.off.saturating_sub(10),
            _ => {}
        }
        Action::None
    }

    pub fn on_key(&mut self, k: KeyEvent, ctx: &Ctx) -> Action {
        if self.usage.is_some() {
            return self.usage_key(k);
        }
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
            KeyCode::Char('p') => self.open_usage(ctx),
            KeyCode::Enter => return self.open_action(),
            KeyCode::Char('x') => return self.xref_action(),
            _ => {}
        }
        Action::None
    }

    pub fn on_mouse(&mut self, m: MouseEvent, area: Rect) {
        if let Some(usage) = &mut self.usage {
            let n = usage.lines.len();
            match m.kind {
                MouseEventKind::ScrollUp => usage.off = usage.off.saturating_sub(3),
                MouseEventKind::ScrollDown => usage.off = (usage.off + 3).min(n.saturating_sub(1)),
                MouseEventKind::Down(_) => self.usage = None,
                _ => {}
            }
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.move_sel(-20),
            MouseEventKind::ScrollDown => self.move_sel(20),
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
        bar.push(crate::ui::crumb_sep());
        bar.push(Span::styled(
            format!(
                "exports  {}/{}  · {} data",
                rows.len(),
                self.items.len(),
                self.data_count()
            ),
            Style::default().add_modifier(Modifier::DIM),
        ));
        crate::ui::render_bar(buf, x0, area.y, w, &bar);

        let state = match self.mode {
            Mode::Search => format!(" /{}", self.filter),
            Mode::Normal if !self.filter.is_empty() => format!(" filter: {}", self.filter),
            Mode::Normal => String::new(),
        };
        let hint = match self.mode {
            Mode::Search => crate::ui::hint_bar(&[
                &[("type", ""), ("↑↓", "pick")],
                &[("Enter", "open"), ("Tab", "list"), ("Esc", "cancel")],
                &[("?", "help")],
            ]),
            Mode::Normal => crate::ui::hint_bar(&[
                &[("j/k", "move"), ("/", "search")],
                &[("p", "uses"), ("Enter", "open"), ("x", "xrefs")],
                &[("m", "menu"), ("v", "list"), ("i", "switch")],
                &[("q", "quit")],
            ]),
        };
        crate::ui::put_str(
            buf,
            x0,
            area.y + 1,
            state,
            w,
            Style::default().add_modifier(Modifier::DIM),
        );
        crate::ui::render_bar(buf, x0, area.y + area.height.saturating_sub(1), w, &hint);

        if rows.is_empty() {
            crate::ui::put_str(
                buf,
                x0 + 2,
                area.y + 3,
                "no exported symbols",
                w.saturating_sub(4),
                Style::default().add_modifier(Modifier::DIM),
            );
            return;
        }

        for (row, &i) in rows.iter().enumerate().skip(self.top).take(listh) {
            let y = area.y + 2 + (row - self.top) as u16;
            let it = &self.items[i];
            let is_sel = row == self.sel;
            let tag = if it.is_data { "  [data]" } else { "" };
            if is_sel {
                let text = format!(
                    "  {:<aw$}  {}{tag}",
                    it.addr,
                    it.display_name,
                    aw = self.awidth
                );
                crate::ui::put_str(
                    buf,
                    x0,
                    y,
                    format!("{text:<w$}"),
                    w,
                    Style::default().add_modifier(Modifier::REVERSED),
                );
                continue;
            }
            let name_style = if it.is_data {
                Style::default().fg(crate::theme::DATA)
            } else {
                Style::default().fg(crate::theme::FUNC)
            };
            let spans = vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("{:<aw$}", it.addr, aw = self.awidth),
                    Style::default()
                        .fg(crate::theme::ADDR)
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(format!("  {}", it.display_name), name_style),
                Span::styled(tag, Style::default().fg(Color::DarkGray)),
            ];
            crate::ui::put_spans(buf, x0, y, w, &spans);
        }

        self.render_usage(area, buf);
    }

    fn render_usage(&self, area: Rect, buf: &mut Buffer) {
        let Some(usage) = &self.usage else { return };
        let bw = area.width.saturating_sub(6).clamp(50, 100);
        let bh = area.height.saturating_sub(4).clamp(8, 30);
        let bx = area.x + (area.width.saturating_sub(bw)) / 2;
        let by = area.y + (area.height.saturating_sub(bh)) / 2;
        crate::ui::draw_box(buf, bx, by, bw, bh, &usage.title);
        let view_h = (bh as usize).saturating_sub(3);
        for (i, line) in usage.lines.iter().skip(usage.off).take(view_h).enumerate() {
            crate::ui::put_str(
                buf,
                bx + 2,
                by + 1 + i as u16,
                line,
                (bw - 4) as usize,
                Style::default().fg(Color::Yellow),
            );
        }
        crate::ui::put_str(
            buf,
            bx + 2,
            by + bh - 1,
            " j/k scroll · Enter/x opens full xrefs · p/q/Esc close ",
            (bw - 4) as usize,
            Style::default().add_modifier(Modifier::DIM),
        );
    }
}
