//! The function picker: an address-ordered list of *all* functions, with a
//! "recently viewed" subsection on top — functions you opened here (`▸ you`)
//! and functions/addresses the agent named in its pane (`◆ agent`), refreshed
//! live. Non-function addresses are annotated with their section + nearest
//! symbol (so a `.bss` global reads as `.bss → srv_state`, not `(addr)`).

use crate::ctx::Ctx;
use crate::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::collections::{HashMap, HashSet};

pub enum Action {
    OpenDecompile(String),
    OpenXrefs(String),
    Switch,
    Quit,
    None,
}

const YOU: u8 = 1;
const AGENT: u8 = 2;
const RECENT_CAP: usize = 12;

#[derive(Clone)]
struct Item {
    addr: String,
    name: String,   // "" => a data address (not a function)
    target: String, // decompile/xref target: fn name, or the address
    import: bool,
    annot: String,  // section → nearest-symbol for data addresses; "" for functions
    src: u8,        // YOU|AGENT bits (0 in the "all" group)
}

/// One display line: a group header, or an item in the recent / all group.
enum Row {
    Header(String),
    Recent(usize),
    All(usize),
}

enum Mode {
    Normal,
    Search,
}

pub struct Picker {
    all: Vec<Item>,     // every function, by address (stable)
    recent: Vec<Item>,  // rebuilt from opens + agent scan
    opens: Vec<String>, // fn names you opened here (MRU, newest first)
    agent: Vec<String>, // tokens from the agent's pane (most-recent first)
    awidth: usize,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sel: usize, // index into the current rows()
    top: usize,
    pending_g: bool,
    // classification + annotation tables (static per target)
    fn_names: HashSet<String>,
    fn_name_addr: HashMap<String, String>, // fn name -> addr
    fn_addr_name: HashMap<String, String>, // addr -> fn name
    imports: HashSet<String>,
    ranges: Vec<(u64, u64, String)>, // section ranges
    syms: Vec<(u64, String)>,        // symbol addr -> name, sorted ascending
    sec_lines: Vec<String>,          // raw `bn sections` for the `s` popup
    sec_off: Option<usize>,
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok()
}

impl Picker {
    pub fn new(ctx: &Ctx) -> Self {
        let awidth = ctx.funcs.iter().map(|f| f.addr.len()).max().unwrap_or(10).max(10);

        let mut all: Vec<Item> = ctx
            .funcs
            .iter()
            .map(|f| Item {
                addr: f.addr.clone(),
                name: f.name.clone(),
                target: f.name.clone(),
                import: ctx.import_names.contains(&f.name),
                annot: String::new(),
                src: 0,
            })
            .collect();
        all.sort_by_key(|it| parse_hex(&it.addr).unwrap_or(0));

        let fn_names = ctx.func_names.clone();
        let fn_name_addr: HashMap<String, String> =
            ctx.funcs.iter().map(|f| (f.name.clone(), f.addr.clone())).collect();
        let fn_addr_name: HashMap<String, String> =
            ctx.funcs.iter().map(|f| (f.addr.clone(), f.name.clone())).collect();

        let mut syms: Vec<(u64, String)> = ctx
            .addr_by_name
            .iter()
            .filter_map(|(n, a)| parse_hex(a).map(|v| (v, n.clone())))
            .collect();
        syms.sort_by_key(|(a, _)| *a);

        let sec_lines = ctx.sections_text.clone();
        let ranges: Vec<(u64, u64, String)> =
            ctx.section_ranges.iter().map(|(s, e, n, _)| (*s, *e, n.clone())).collect();

        Picker {
            all,
            recent: Vec::new(),
            opens: Vec::new(),
            agent: Vec::new(),
            awidth,
            filter: String::new(),
            prev_filter: String::new(),
            mode: Mode::Normal,
            sel: 0,
            top: 0,
            pending_g: false,
            fn_names,
            fn_name_addr,
            fn_addr_name,
            imports: ctx.import_names.clone(),
            ranges,
            syms,
            sec_lines,
            sec_off: None,
        }
    }

    /// Record a function you just opened in the lens (MRU, newest first).
    pub fn record_open(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        self.opens.retain(|n| n != name);
        self.opens.insert(0, name.to_string());
        self.opens.truncate(RECENT_CAP);
        self.rebuild_recent();
    }

    /// Update the agent-referenced tokens (from a fresh scan of its pane).
    pub fn update_agent(&mut self, tokens: Vec<String>) {
        if tokens == self.agent {
            return;
        }
        self.agent = tokens;
        self.rebuild_recent();
    }

    /// Section + nearest-symbol annotation for a data address.
    fn annotate(&self, addr: u64) -> String {
        let sec = self.ranges.iter().find(|(s, e, _)| addr >= *s && addr < *e);
        let sec_start = sec.map(|(s, _, _)| *s).unwrap_or(0);
        // nearest symbol at or below the address, but within the same section
        let sym = self.syms.iter().rev().find(|(a, _)| *a <= addr && *a >= sec_start);
        match (sec, sym) {
            (Some((_, _, name)), Some((sa, sn))) => {
                let off = addr - sa;
                if off == 0 {
                    format!("{name} → {sn}")
                } else {
                    format!("{name} → {sn} +{off:#x}")
                }
            }
            (Some((_, _, name)), None) => name.clone(),
            (None, _) => "(no section)".to_string(),
        }
    }

    /// Rebuild the recent group: your opens (newest first), then agent-referenced
    /// items not already shown, in scan (recency) order. Capped at RECENT_CAP.
    fn rebuild_recent(&mut self) {
        let agent_set: HashSet<&str> = self.agent.iter().map(String::as_str).collect();
        let mut out: Vec<Item> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        // 1. your lens opens (functions), newest first
        for name in &self.opens {
            if !self.fn_names.contains(name) || !seen.insert(format!("f:{name}")) {
                continue;
            }
            let addr = self.fn_name_addr.get(name).cloned().unwrap_or_default();
            let mut src = YOU;
            if agent_set.contains(name.as_str()) || agent_set.contains(addr.as_str()) {
                src |= AGENT;
            }
            out.push(Item {
                addr,
                name: name.clone(),
                target: name.clone(),
                import: self.imports.contains(name),
                annot: String::new(),
                src,
            });
        }

        // 2. agent-referenced tokens, in recency order
        for tok in &self.agent {
            if out.len() >= RECENT_CAP {
                break;
            }
            if self.fn_names.contains(tok) {
                if !seen.insert(format!("f:{tok}")) {
                    continue;
                }
                out.push(Item {
                    addr: self.fn_name_addr.get(tok).cloned().unwrap_or_default(),
                    name: tok.clone(),
                    target: tok.clone(),
                    import: self.imports.contains(tok),
                    annot: String::new(),
                    src: AGENT,
                });
            } else if tok.starts_with("0x") {
                if let Some(fname) = self.fn_addr_name.get(tok) {
                    if !seen.insert(format!("f:{fname}")) {
                        continue;
                    }
                    out.push(Item {
                        addr: tok.clone(),
                        name: fname.clone(),
                        target: fname.clone(),
                        import: self.imports.contains(fname),
                        annot: String::new(),
                        src: AGENT,
                    });
                } else if seen.insert(format!("a:{tok}")) {
                    let annot = parse_hex(tok).map(|v| self.annotate(v)).unwrap_or_default();
                    out.push(Item {
                        addr: tok.clone(),
                        name: String::new(),
                        target: tok.clone(),
                        import: false,
                        annot,
                        src: AGENT,
                    });
                }
            }
            // a plain non-function identifier is noise — skip it
        }

        out.truncate(RECENT_CAP);
        self.recent = out;
    }

    fn filtered_all(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        let mut v: Vec<usize> = (0..self.all.len())
            .filter(|&i| {
                f.is_empty()
                    || self.all[i].name.to_lowercase().contains(&f)
                    || self.all[i].addr.contains(&f)
            })
            .collect();
        if !f.is_empty() {
            // rank by match quality on the name so `main` beats `__libc_start_main`
            v.sort_by_key(|&i| {
                let name = self.all[i].name.to_lowercase();
                if name == f {
                    0
                } else if name.starts_with(&f) {
                    1
                } else if name.contains(&f) {
                    2
                } else {
                    3
                }
            });
        }
        v
    }

    /// The current display rows: grouped (recent + all) with no filter, or a
    /// flat ranked list of matches when filtering.
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if self.filter.is_empty() {
            if !self.recent.is_empty() {
                rows.push(Row::Header("recent  ·  ▸ you   ◆ agent   ★ both".into()));
                rows.extend((0..self.recent.len()).map(Row::Recent));
            }
            rows.push(Row::Header("all functions · by address".into()));
            rows.extend((0..self.all.len()).map(Row::All));
        } else {
            rows.extend(self.filtered_all().into_iter().map(Row::All));
        }
        rows
    }

    fn sel_positions(rows: &[Row]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| !matches!(r, Row::Header(_)))
            .map(|(i, _)| i)
            .collect()
    }

    /// Ensure `sel` points at a selectable row (never a header / out of range).
    fn snap(&mut self, rows: &[Row]) {
        let sp = Self::sel_positions(rows);
        if sp.is_empty() {
            self.sel = 0;
        } else if !sp.contains(&self.sel) {
            self.sel = *sp.iter().find(|&&i| i >= self.sel).unwrap_or(sp.last().unwrap());
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let rows = self.rows();
        let sp = Self::sel_positions(&rows);
        if sp.is_empty() {
            return;
        }
        let cur = sp.iter().position(|&i| i == self.sel).unwrap_or(0) as i64;
        let ncur = (cur + delta).clamp(0, sp.len() as i64 - 1) as usize;
        self.sel = sp[ncur];
    }

    fn current_target(&self) -> Option<String> {
        match self.rows().get(self.sel)? {
            Row::Recent(i) => Some(self.recent[*i].target.clone()),
            Row::All(i) => Some(self.all[*i].target.clone()),
            Row::Header(_) => None,
        }
    }

    fn draw_item(&self, buf: &mut Buffer, x0: u16, y: u16, w: usize, it: &Item, is_sel: bool, recent: bool) {
        let glyph = if recent {
            match (it.src & YOU != 0, it.src & AGENT != 0) {
                (true, true) => "★ ",
                (true, false) => "▸ ",
                (false, true) => "◆ ",
                _ => "  ",
            }
        } else {
            "  "
        };
        let tail = if it.name.is_empty() {
            if it.annot.is_empty() { "(addr)".to_string() } else { it.annot.clone() }
        } else {
            it.name.clone()
        };
        if is_sel {
            let text = format!("{glyph}{:<aw$}  {tail}", it.addr, aw = self.awidth);
            buf.set_stringn(x0, y, format!("{text:<w$}"), w, Style::default().add_modifier(Modifier::REVERSED));
            return;
        }
        let dim = if it.import { Modifier::DIM } else { Modifier::empty() };
        let tail_style = if it.name.is_empty() {
            Style::default().fg(Color::Magenta) // data annotation
        } else {
            Style::default().fg(theme::NAME).add_modifier(dim)
        };
        let spans = vec![
            Span::styled(glyph.to_string(), Style::default().fg(theme::MARK)),
            Span::styled(format!("{:<aw$}", it.addr, aw = self.awidth), Style::default().fg(theme::ADDR).add_modifier(dim)),
            Span::styled(format!("  {tail}"), tail_style),
        ];
        crate::ui::put_spans(buf, x0, y, w, &spans);
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &Ctx) {
        let rows = self.rows();
        self.snap(&rows);
        let listh = area.height.saturating_sub(3) as usize;
        if self.sel < self.top {
            self.top = self.sel;
        }
        if listh > 0 && self.sel >= self.top + listh {
            self.top = self.sel + 1 - listh;
        }

        let x0 = area.x;
        let w = area.width as usize;
        let shown = if self.filter.is_empty() { self.all.len() } else { self.filtered_all().len() };
        let mut bar = crate::ui::crumbs(ctx);
        bar.push(Span::styled(
            format!("   {}/{}", shown, self.all.len()),
            Style::default().add_modifier(Modifier::DIM),
        ));
        crate::ui::render_bar(buf, x0, area.y, w, &bar);
        let (state, keys) = match self.mode {
            Mode::Search => (
                format!(" /{}", self.filter),
                " type · ↑↓ pick · Enter open · Tab list · Esc cancel",
            ),
            Mode::Normal => (
                if self.filter.is_empty() {
                    " (no filter)".to_string()
                } else {
                    format!(" filter: {}", self.filter)
                },
                " j/k · g/G · / search · Enter decompile · x xrefs · s sects · i switch bn · q quit",
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

        for (ri, row) in rows.iter().enumerate().skip(self.top).take(listh) {
            let y = area.y + 2 + (ri - self.top) as u16;
            match row {
                Row::Header(h) => {
                    let label = format!("── {h} ");
                    let pad = w.saturating_sub(label.chars().count());
                    let line = format!("{label}{}", "─".repeat(pad));
                    buf.set_stringn(x0, y, line, w, Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM));
                }
                Row::Recent(i) => self.draw_item(buf, x0, y, w, &self.recent[*i], ri == self.sel, true),
                Row::All(i) => self.draw_item(buf, x0, y, w, &self.all[*i], ri == self.sel, false),
            }
        }

        // section overlay on top of the list
        if let Some(off) = self.sec_off {
            let bw = (area.width.saturating_sub(6)).clamp(50, 100);
            let bh = (area.height.saturating_sub(4)).clamp(8, 30);
            let bx = area.x + (area.width.saturating_sub(bw)) / 2;
            let by = area.y + (area.height.saturating_sub(bh)) / 2;
            crate::ui::draw_box(buf, bx, by, bw, bh, "sections  ·  r-x=exec  rw-=data  w+x flagged");
            let view_h = (bh as usize).saturating_sub(3);
            for (i, ln) in self.sec_lines.iter().skip(off).take(view_h).enumerate() {
                buf.set_stringn(bx + 2, by + 1 + i as u16, ln, (bw - 4) as usize, Style::default().fg(Color::Yellow));
            }
            buf.set_stringn(bx + 2, by + bh - 1, " j/k scroll · s/q close ", (bw - 4) as usize, Style::default().add_modifier(Modifier::DIM));
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent, area: Rect) {
        if self.sec_off.is_some() {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.move_sel(-3),
            MouseEventKind::ScrollDown => self.move_sel(3),
            MouseEventKind::Down(_) => {
                let rows = self.rows();
                let ri = self.top + m.row.saturating_sub(area.y + 2) as usize;
                if matches!(rows.get(ri), Some(Row::Recent(_)) | Some(Row::All(_))) {
                    self.sel = ri;
                }
            }
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Action {
        // section overlay captures keys while open
        if let Some(off) = self.sec_off {
            let n = self.sec_lines.len();
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('s') | KeyCode::Enter => {
                    self.sec_off = None
                }
                KeyCode::Char('j') | KeyCode::Down => self.sec_off = Some((off + 1).min(n.saturating_sub(1))),
                KeyCode::Char('k') | KeyCode::Up => self.sec_off = Some(off.saturating_sub(1)),
                KeyCode::PageDown => self.sec_off = Some((off + 10).min(n.saturating_sub(1))),
                KeyCode::PageUp => self.sec_off = Some(off.saturating_sub(10)),
                _ => {}
            }
            return Action::None;
        }

        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Mode::Search = self.mode {
            match k.code {
                // Enter opens the highlighted (top-ranked) match directly.
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    if let Some(t) = self.current_target() {
                        return Action::OpenDecompile(t);
                    }
                }
                // Tab keeps the filter but drops to normal mode.
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

        // normal mode
        if self.pending_g {
            self.pending_g = false;
            if k.code == KeyCode::Char('g') {
                self.move_sel(i64::MIN / 2);
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
            KeyCode::Enter => {
                if let Some(t) = self.current_target() {
                    return Action::OpenDecompile(t);
                }
            }
            KeyCode::Char('x') => {
                if let Some(t) = self.current_target() {
                    return Action::OpenXrefs(t);
                }
            }
            KeyCode::Char('s') => self.sec_off = Some(0),
            KeyCode::Char('i') => return Action::Switch,
            _ => {}
        }
        Action::None
    }
}
