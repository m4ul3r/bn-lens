//! The strings view: an address-ordered, filterable list of every string bn
//! recovered. `Enter`/`x` cross-references the selected string (its uses land in
//! the viewer), so it doubles as a "where is this text used?" entry point.

use crate::ctx::Ctx;
use crate::decomp::{addr_lines, lines_at};
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
    awidth: usize,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sel: usize,
    top: usize,
    pending_g: bool,
    usage: Option<Usage>,
}

/// Caps that bound a `p` peek's latency: one decompile per referencing function
/// (~hundreds of ms each) and a total site count. The full set is one `x` away.
const MAX_SITES: usize = 12;
const MAX_FUNCS: usize = 6;

fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Parse `bn xrefs` text into ([(function, [callsite addrs])], [data-ref lines]).
/// Code lines look like `0x<fa>  <name>  (N sites: 0x.., 0x..)`; data-ref lines
/// are kept verbatim (their exact shape varies, and we only display them).
fn parse_xrefs(text: &str) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let mut code: Vec<(String, Vec<String>)> = Vec::new();
    let mut data: Vec<String> = Vec::new();
    let mut section = 0u8; // 1 = code refs, 2 = data refs
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("code refs") {
            section = 1;
            continue;
        }
        if t.starts_with("data refs") {
            section = 2;
            continue;
        }
        if t.is_empty() || t == "- none" || t.starts_with("xrefs to") {
            continue;
        }
        match section {
            1 => {
                let mut toks = t.split_whitespace();
                let _func_addr = toks.next();
                let name = toks.next().unwrap_or("").to_string();
                let sites: Vec<String> = toks
                    .filter_map(|w| {
                        let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                        w.starts_with("0x").then(|| w.to_string())
                    })
                    .collect();
                if !name.is_empty() {
                    code.push((name, sites));
                }
            }
            2 => data.push(t.to_string()),
            _ => {}
        }
    }
    (code, data)
}

/// The instruction line at `addr` (first non-comment line of a 1-instruction
/// linear disasm), trimmed; falls back to the bare address on any miss.
fn disasm_line(ctx: &Ctx, addr: &str) -> String {
    ctx.bn
        .disasm_linear(addr, 1)
        .lines()
        .map(str::trim_end)
        .find(|l| !l.trim_start().starts_with("//") && !l.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| addr.to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_xrefs;

    #[test]
    fn parses_code_callsites_and_data_refs() {
        let text = "\
xrefs to 0x4016d0 (5 code, 1 data)

code refs: 5 sites across 2 functions
  0x40d040  add_resource_record  (2 sites: 0x40d214, 0x40d620)
  0x410240  expand_buf  (3 sites: 0x41028c, 0x4102a0, 0x4102b4)

data refs:
  0x4152a0  .data  ptr_table
";
        let (code, data) = parse_xrefs(text);
        assert_eq!(code.len(), 2);
        assert_eq!(code[0].0, "add_resource_record");
        assert_eq!(code[0].1, vec!["0x40d214", "0x40d620"]);
        assert_eq!(code[1].0, "expand_buf");
        assert_eq!(code[1].1.len(), 3);
        assert_eq!(code[1].1[2], "0x4102b4");
        assert_eq!(data, vec!["0x4152a0  .data  ptr_table".to_string()]);
    }

    #[test]
    fn handles_no_references() {
        let text = "\
xrefs to 0x400238 (0 code, 0 data)

code refs:
- none

data refs:
- none
";
        let (code, data) = parse_xrefs(text);
        assert!(code.is_empty());
        assert!(data.is_empty());
    }
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok()
}

impl StringsList {
    pub fn new(ctx: &Ctx) -> Self {
        let items = Self::build(ctx);
        let awidth = items.iter().map(|it| it.addr.len()).max().unwrap_or(10).max(10);
        StringsList {
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

    /// Re-pull strings from a rebuilt ctx (keeps the filter, snaps the cursor).
    pub fn refresh(&mut self, ctx: &Ctx) {
        self.items = Self::build(ctx);
        self.awidth = self.items.iter().map(|it| it.addr.len()).max().unwrap_or(10).max(10);
    }

    /// Deduped by address, sorted by address. bn emits the same content at
    /// several addresses (`.rodata`/`.dynstr`); we keep each distinct address.
    fn build(ctx: &Ctx) -> Vec<StrItem> {
        let mut seen = std::collections::HashSet::new();
        let mut items: Vec<StrItem> = ctx
            .bn
            .strings()
            .into_iter()
            .filter(|(_, addr)| addr.starts_with("0x") && seen.insert(addr.clone()))
            .map(|(content, addr)| StrItem { addr, content })
            .collect();
        items.sort_by_key(|it| parse_hex(&it.addr).unwrap_or(0));
        items
    }

    fn filtered(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| {
                f.is_empty()
                    || self.items[i].content.to_lowercase().contains(&f)
                    || self.items[i].addr.contains(&f)
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

    fn current(&self) -> Option<&StrItem> {
        let rows = self.filtered();
        rows.get(self.sel).map(|&i| &self.items[i])
    }

    fn current_addr(&self) -> Option<String> {
        self.current().map(|it| it.addr.clone())
    }

    /// `p`: peek *where the string is used in code or data*. Parse `bn xrefs`,
    /// decompile each referencing function once (`--addresses`), and show the
    /// pseudo-C statement at each callsite (grouped by function), plus any data
    /// refs. Falls back to the disassembled instruction if a site maps to no
    /// decompiled line. (`x`/Enter opens the full navigable xrefs listing.)
    fn open_usage(&mut self, ctx: &Ctx) {
        let Some(item) = self.current() else { return };
        let (addr, content) = (item.addr.clone(), item.content.clone());
        let (code, data) = parse_xrefs(&ctx.bn.xrefs(&addr));

        let mut lines: Vec<String> = Vec::new();
        if code.is_empty() && data.is_empty() {
            lines.push("no code or data references".into());
        }
        if !code.is_empty() {
            lines.push("code:".into());
            let mut sites_left = MAX_SITES;
            let mut funcs_left = MAX_FUNCS;
            'outer: for (func, sites) in &code {
                if funcs_left == 0 {
                    lines.push("  … more functions — x for the full list".into());
                    break;
                }
                funcs_left -= 1;
                lines.push(format!("  {func}"));
                let dec = addr_lines(&ctx.bn.decompile_addr(func));
                for site in sites {
                    if sites_left == 0 {
                        lines.push("    … more sites — x for the full list".into());
                        break 'outer;
                    }
                    sites_left -= 1;
                    let matched = crate::ctx::parse_hex(site)
                        .map(|cs| lines_at(&dec, cs))
                        .unwrap_or_default();
                    if matched.is_empty() {
                        lines.push(format!("    {site}  {}", disasm_line(ctx, site)));
                    } else {
                        for (i, text) in matched.iter().take(2).enumerate() {
                            if i == 0 {
                                lines.push(format!("    {site}  {text}"));
                            } else {
                                lines.push(format!("               {text}"));
                            }
                        }
                    }
                }
            }
        }
        if !data.is_empty() {
            if !code.is_empty() {
                lines.push(String::new());
            }
            lines.push("data:".into());
            lines.extend(data.into_iter().map(|d| format!("  {d}")));
        }

        self.usage = Some(Usage {
            title: format!("used in code · \"{}\"", ellipsize(&content, 34)),
            addr,
            lines,
            off: 0,
        });
    }

    fn usage_key(&mut self, k: KeyEvent) -> Action {
        let Some(usage) = &mut self.usage else { return Action::None };
        let n = usage.lines.len();
        match k.code {
            KeyCode::Enter | KeyCode::Char('x') => {
                let addr = usage.addr.clone();
                self.usage = None;
                return Action::OpenXrefs(addr);
            }
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('p') => self.usage = None,
            KeyCode::Char('j') | KeyCode::Down => usage.off = (usage.off + 1).min(n.saturating_sub(1)),
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
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
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
            format!("   strings  {}/{}", rows.len(), self.items.len()),
            Style::default().add_modifier(Modifier::DIM),
        ));
        crate::ui::render_bar(buf, x0, area.y, w, &bar);

        let (state, keys) = match self.mode {
            Mode::Search => (
                format!(" /{}", self.filter),
                " type · ↑↓ pick · Enter xrefs · Tab list · Esc cancel · ? help",
            ),
            Mode::Normal => (
                if self.filter.is_empty() {
                    " strings · (no filter)".to_string()
                } else {
                    format!(" strings · filter: {}", self.filter)
                },
                " j/k move · / search · p usage · Enter/x xrefs · m menu · ? help · q quit",
            ),
        };
        buf.set_stringn(x0, area.y + 1, state, w, Style::default().add_modifier(Modifier::DIM));
        crate::ui::render_bar(
            buf,
            x0,
            area.y + area.height.saturating_sub(1),
            w,
            &[Span::styled(keys, Style::default().add_modifier(Modifier::DIM))],
        );

        for (row, &i) in rows.iter().enumerate().skip(self.top).take(listh) {
            let y = area.y + 2 + (row - self.top) as u16;
            let it = &self.items[i];
            let is_sel = row == self.sel;
            if is_sel {
                let text = format!("{:<aw$}  \"{}\"", it.addr, it.content, aw = self.awidth);
                buf.set_stringn(x0, y, format!("{text:<w$}"), w, Style::default().add_modifier(Modifier::REVERSED));
                continue;
            }
            let spans = vec![
                Span::styled(format!("{:<aw$}", it.addr, aw = self.awidth), Style::default().fg(crate::theme::ADDR).add_modifier(Modifier::DIM)),
                Span::styled(format!("  \"{}\"", it.content), Style::default().fg(Color::Magenta)),
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
            buf.set_stringn(
                bx + 2,
                by + 1 + i as u16,
                line,
                (bw - 4) as usize,
                Style::default().fg(Color::Yellow),
            );
        }
        buf.set_stringn(
            bx + 2,
            by + bh - 1,
            " j/k scroll · Enter/x opens full xrefs · p/q/Esc close ",
            (bw - 4) as usize,
            Style::default().add_modifier(Modifier::DIM),
        );
    }
}
