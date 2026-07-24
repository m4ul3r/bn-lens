//! The strings view: an address-ordered, filterable list of every string bn
//! recovered. `Enter`/`x` cross-references the selected string (its uses land in
//! the viewer), so it doubles as a "where is this text used?" entry point.

use crate::ctx::Ctx;
use crate::picker::Action;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

struct StrItem {
    addr: String,
    content: String,
}

enum Mode {
    Normal,
    Search,
}

/// A scrollable overlay showing where a string is used (raw `bn xrefs`).
struct Usage {
    title: String,
    addr: String,
    lines: Vec<String>,
    off: usize,
}

pub struct StringsList {
    items: Vec<StrItem>,
    /// Backend read failure, including a payload whose `items` field would not
    /// decode — see [`crate::picker::list_error`]. An empty string table is a
    /// claim about the binary and must not stand in for a failed read.
    error: Option<String>,
    awidth: usize,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sel: usize,
    top: usize,
    pending_g: bool,
    usage: Option<Usage>,
}

fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok()
}

impl StringsList {
    pub fn new(ctx: &Ctx) -> Self {
        let (items, read_error) = Self::build(ctx);
        let error = crate::picker::list_error(items.is_empty(), read_error, ctx.bn.last_error());
        let awidth = items
            .iter()
            .map(|it| it.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);
        StringsList {
            items,
            error,
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

    /// Re-pull strings from a rebuilt ctx (keeps the filter, snaps the cursor).
    pub fn refresh(&mut self, ctx: &Ctx) {
        let (items, read_error) = Self::build(ctx);
        self.items = items;
        self.error =
            crate::picker::list_error(self.items.is_empty(), read_error, ctx.bn.last_error());
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
        // The list underneath moved; a surviving popup would describe the
        // pre-refresh selection.
        self.usage = None;
    }

    /// Deduped by address. bn emits the same content at several addresses
    /// (`.rodata`/`.dynstr`); we keep each distinct address. Ordered for triage:
    /// real `.rodata` literals first, symbol-table/header noise last (by section
    /// rank), then by address within each rank — so the useful strings lead
    /// instead of the ELF header junk a pure address sort surfaces first.
    fn build(ctx: &Ctx) -> (Vec<StrItem>, Option<String>) {
        let mut seen = std::collections::HashSet::new();
        let listing = ctx.bn.strings();
        let mut items: Vec<StrItem> = listing
            .items
            .into_iter()
            .filter(|(_, addr)| addr.starts_with("0x") && seen.insert(addr.clone()))
            .map(|(content, addr)| StrItem { addr, content })
            .collect();
        items.sort_by_key(|it| {
            let addr = parse_hex(&it.addr).unwrap_or(0);
            (ctx.string_rank(addr), addr)
        });
        (items, listing.error)
    }

    fn filtered(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                f.is_empty() || it.content.to_lowercase().contains(&f) || it.addr.contains(&f)
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

    /// True while the search filter is capturing raw text (App must not steal
    /// `m`/`?`/`^R`).
    pub fn is_searching(&self) -> bool {
        matches!(self.mode, Mode::Search)
    }

    /// True while the usage popup owns input.
    pub fn popup_open(&self) -> bool {
        self.usage.is_some()
    }

    fn current(&self) -> Option<&StrItem> {
        let rows = self.filtered();
        rows.get(self.sel).map(|&i| &self.items[i])
    }

    fn current_addr(&self) -> Option<String> {
        self.current().map(|it| it.addr.clone())
    }

    /// `p`: peek *where the string is used in code or data*. Parse `bn xrefs`,
    /// decompile each referencing function once (`--addresses`), and show exact
    /// disassembly plus approximate pseudo-C at each callsite, grouped by
    /// function, plus any data refs. (`x`/Enter opens the full xrefs listing.)
    /// Opens the popup shell immediately (loading line); the app computes the
    /// report on a worker thread and delivers it via [`Self::set_usage_lines`].
    fn open_usage(&mut self) -> Action {
        let Some(item) = self.current() else {
            return Action::None;
        };
        let (addr, content) = (item.addr.clone(), item.content.clone());
        self.usage = Some(Usage {
            title: format!("used in code · \"{}\"", ellipsize(&content, 34)),
            lines: crate::usage::loading_lines(0.0),
            addr: addr.clone(),
            off: 0,
        });
        Action::PeekUsage {
            addr,
            hint: content,
        }
    }

    /// Fill the open usage popup with the worker's report. Returns false —
    /// and drops the lines — if the popup was closed or re-targeted meanwhile.
    pub fn set_usage_lines(&mut self, addr: &str, lines: Vec<String>) -> bool {
        match &mut self.usage {
            Some(usage) if usage.addr == addr => {
                usage.off = usage.off.min(lines.len().saturating_sub(1));
                usage.lines = lines;
                true
            }
            _ => false,
        }
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

    pub fn on_key(&mut self, k: KeyEvent, _ctx: &Ctx) -> Action {
        if self.usage.is_some() {
            return self.usage_key(k);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Mode::Search = self.mode {
            match k.code {
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    if let Some(a) = self.current_addr() {
                        return Action::OpenXrefs(a);
                    }
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
                // A ctrl chord is never text: crossterm decodes ^U as
                // `Char('u') + CONTROL`, so an unguarded arm turns a line-edit
                // reflex into a literal letter in the filter.
                KeyCode::Char(c) if !ctrl => {
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
                // Esc clears the filter if set, else returns to Symbols (never
                // closes the pane).
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
            KeyCode::Char('p') => return self.open_usage(),
            KeyCode::Enter | KeyCode::Char('x') => {
                if let Some(a) = self.current_addr() {
                    return Action::OpenXrefs(a);
                }
            }
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
            format!("strings  {}/{}", rows.len(), self.items.len()),
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
                &[("Enter", "xrefs"), ("Tab", "list"), ("Esc", "cancel")],
                &[("?", "help")],
            ]),
            Mode::Normal => crate::ui::hint_bar(&[
                &[("j/k", "move"), ("/", "search")],
                &[("p", "usage"), ("Enter/x", "xrefs")],
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

        // A failed read must dominate the (necessarily empty) table — an empty
        // strings list is a real finding, and a bridge failure must not borrow
        // its authority.
        if let Some(error) = &self.error {
            crate::ui::put_str(
                buf,
                x0 + 2,
                area.y + 3,
                format!("✗ {error}"),
                w.saturating_sub(4),
                Style::default().fg(Color::Red),
            );
            self.render_usage(area, buf);
            return;
        }

        for (row, &i) in rows.iter().enumerate().skip(self.top).take(listh) {
            let y = area.y + 2 + (row - self.top) as u16;
            let it = &self.items[i];
            let is_sel = row == self.sel;
            if is_sel {
                let text = format!("{:<aw$}  \"{}\"", it.addr, it.content, aw = self.awidth);
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
            let spans = vec![
                Span::styled(
                    format!("{:<aw$}", it.addr, aw = self.awidth),
                    Style::default()
                        .fg(crate::theme::ADDR)
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  \"{}\"", it.content),
                    Style::default().fg(Color::Magenta),
                ),
            ];
            crate::ui::put_spans(buf, x0, y, w, &spans);
        }

        self.render_usage(area, buf);
    }

    /// The `p` overlay: raw `bn xrefs` for the selected string — its callsites.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::tests::{ctrl, plain};

    #[test]
    fn a_failed_read_surfaces_an_error_instead_of_an_empty_table() {
        // `Bn::strings` swallows its error into the shared health cell; the view
        // has to recover it, or a wedged bridge reads as "this binary has no
        // strings".
        let ctx = Ctx::stub();
        let list = StringsList::new(&ctx);
        assert!(list.items.is_empty());
        assert!(
            list.error.is_some_and(|error| !error.is_empty()),
            "an empty strings list under a recorded read failure must show it"
        );
    }

    #[test]
    fn ctrl_chords_do_not_type_into_the_filter() {
        let ctx = Ctx::stub();
        let mut list = StringsList::new(&ctx);
        list.on_key(plain('/'), &ctx);
        for ch in "passwd".chars() {
            list.on_key(plain(ch), &ctx);
        }
        assert_eq!(list.filter, "passwd");
        for ch in ['a', 'e', 'u', 'w', 'r'] {
            list.on_key(ctrl(ch), &ctx);
        }
        assert_eq!(
            list.filter, "passwd",
            "a ctrl chord must not push its letter into the filter"
        );
    }

    #[test]
    fn refresh_closes_a_stale_usage_popup() {
        let ctx = Ctx::stub();
        let mut list = StringsList::new(&ctx);
        list.usage = Some(Usage {
            title: "used in code · \"cfg reload\"".into(),
            addr: "0x4a1200".into(),
            lines: vec!["0x401980  apply_config".into()],
            off: 0,
        });
        assert!(list.popup_open());
        list.refresh(&ctx);
        assert!(
            !list.popup_open(),
            "a refresh moves the list; the popup would describe the old selection"
        );
    }

    #[test]
    fn ellipsize_caps_at_the_char_budget() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("0123456789abc", 10), "0123456789…");
    }
}
