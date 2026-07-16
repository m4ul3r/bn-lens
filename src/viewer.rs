//! The code viewer: syntax-highlighted, wrapped decompile/xrefs with a line
//! cursor, function goto (`g`) / data peek (`p`), xrefs (`x`), back stack (`b`),
//! visual-line select (`V`) + ask-the-agent (`?`).

use crate::ctx::Ctx;
use crate::herdr;
use crate::syntax::{self, Line, Tok};
use crate::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

const GUT: u16 = 7; // "NNNN │ "

#[derive(Clone, Copy, PartialEq)]
enum HotKind {
    Func,  // a function symbol  → goto / xrefs
    Data,  // a data global      → peek / xrefs
    Addr,  // a raw 0x… inside a mapped section → peek (or goto if code)
    Local, // a function-local variable/param → highlight uses / type / rename
    Str,   // a string literal → peek backing bytes / xref (resolved via bn strings)
}

/// An interactive token in the decompile: one syntax segment promoted to a
/// typed, actionable region.
struct Hotspot {
    line: usize,
    col: usize,
    target: String, // fn/data name, or the 0x-address for Addr
    kind: HotKind,
    code: bool,     // Addr: inside an executable section (goto vs peek)
}

/// Which rendering of the current function/target we're showing. The three code
/// views cycle with `i`/`I`; Xrefs is entered with `x`.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Decomp,
    Mlil,
    Disasm,
    Xrefs,
}

impl View {
    fn is_code(self) -> bool {
        !matches!(self, View::Xrefs)
    }
    fn label(self) -> &'static str {
        match self {
            View::Decomp => "decompile",
            View::Mlil => "mlil",
            View::Disasm => "disasm",
            View::Xrefs => "xrefs",
        }
    }
}

struct Frame {
    name: String,
    view: View,
    top: usize,
    cline: usize,
}

enum Popup {
    None,
    Ask { label: String, preview: String, prefix: String, buf: String },
    Peek { title: String, lines: Vec<String>, off: usize },
    Rename { old: String, buf: String, err: String },
}

/// A valid C identifier: `[A-Za-z_][A-Za-z0-9_]*` (rejects spaces etc.).
fn valid_ident(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

pub enum Exit {
    Stay,
    Back,
}

pub struct Viewer {
    name: String,
    view: View,
    lines: Vec<Line>,
    spans: Vec<Hotspot>,
    locals: std::collections::HashMap<String, String>, // name -> type (this fn)
    active: Option<usize>, // Tab/click-selected span (else derived from cline)
    top: usize,
    cline: usize,
    status: String,
    vmode: bool,
    vanchor: usize,
    search: String,                  // committed query (for n/N)
    search_input: Option<String>,    // Some while typing after `/`
    stack: Vec<Frame>,
    popup: Popup,
    screen_tgts: Vec<(u16, u16, u16, usize)>, // x0,x1,y,target_idx (for mouse)
}

impl Viewer {
    pub fn new(ctx: &Ctx, name: String, is_code: bool) -> Self {
        let mut v = Viewer {
            name,
            view: if is_code { View::Decomp } else { View::Xrefs },
            lines: Vec::new(),
            spans: Vec::new(),
            locals: std::collections::HashMap::new(),
            active: None,
            top: 0,
            cline: 0,
            status: String::new(),
            vmode: false,
            vanchor: 0,
            search: String::new(),
            search_input: None,
            stack: Vec::new(),
            popup: Popup::None,
            screen_tgts: Vec::new(),
        };
        v.load(ctx);
        v
    }

    fn load(&mut self, ctx: &Ctx) {
        let text = match self.view {
            View::Decomp => ctx.bn.decompile(&self.name),
            View::Mlil => ctx.bn.il(&self.name, "mlil"),
            View::Disasm => ctx.bn.disasm(&self.name),
            View::Xrefs => ctx.bn.xrefs(&self.name),
        };
        self.lines = if matches!(self.view, View::Decomp) {
            syntax::tokenize_c(&text)
        } else {
            syntax::tokenize_plain(&text)
        };
        // locals are meaningful for the named views (decomp/mlil), not disasm/xrefs
        self.locals = if matches!(self.view, View::Decomp | View::Mlil) {
            ctx.bn.local_list(&self.name)
        } else {
            std::collections::HashMap::new()
        };
        let current = if self.view.is_code() { Some(self.name.as_str()) } else { None };
        self.spans = build_spans(&self.lines, ctx, &self.locals, current);
        self.active = None;
    }

    /// The effective selected span: the Tab/click-selected one if it's still on
    /// the cursor line, else the first span on the cursor line.
    fn cur_span(&self) -> Option<usize> {
        if let Some(i) = self.active {
            if self.spans.get(i).is_some_and(|s| s.line == self.cline) {
                return Some(i);
            }
        }
        self.spans.iter().position(|s| s.line == self.cline)
    }

    fn line_text(&self, li: usize) -> String {
        self.lines
            .get(li)
            .map(|segs| segs.iter().map(|s| s.text.clone()).collect())
            .unwrap_or_default()
    }

    /// `g`/`Enter`: act on the selected span by kind — goto a function/code
    /// address, peek a data symbol/address.
    fn act_primary(&mut self, ctx: &Ctx) {
        let Some(i) = self.cur_span() else { return };
        let span = &self.spans[i];
        match span.kind {
            HotKind::Func => {
                let t = span.target.clone();
                self.goto_to(ctx, t);
            }
            HotKind::Addr if span.code => {
                let t = span.target.clone();
                self.goto_to(ctx, t);
            }
            HotKind::Data | HotKind::Addr => {
                let t = span.target.clone();
                self.peek(ctx, &t);
            }
            HotKind::Local => self.show_local(i),
            HotKind::Str => self.peek_string(ctx, i),
        }
    }

    /// Peek a string literal: resolve its content to a `.rodata` address, then
    /// dump the bytes there.
    fn peek_string(&mut self, ctx: &Ctx, i: usize) {
        let content = self.spans[i].target.clone();
        match ctx.strings().get(&content) {
            Some(addr) => {
                let addr = addr.clone();
                self.show_peek(ctx, &format!("\"{}\"", ellipsize(&content, 40)), &addr);
            }
            None => self.status = " ✗ couldn't resolve string (not in .rodata)".into(),
        }
    }

    /// Show a local's type in the status line (its occurrences highlight in the
    /// body while it's selected).
    fn show_local(&mut self, i: usize) {
        let name = self.spans[i].target.clone();
        let ty = self.locals.get(&name).cloned().unwrap_or_else(|| "?".into());
        self.status = format!(" {name} : {ty}   (local · r to rename)");
    }

    /// `r` on a selected local: open the rename dialog.
    fn open_rename(&mut self) {
        let Some(i) = self.cur_span() else { return };
        if self.spans[i].kind != HotKind::Local {
            self.status = " ✗ rename applies to a selected local".into();
            return;
        }
        self.popup = Popup::Rename {
            old: self.spans[i].target.clone(),
            buf: String::new(),
            err: String::new(),
        };
    }

    fn rename_key(&mut self, k: KeyEvent, ctx: &Ctx) {
        let Popup::Rename { old, buf, err } = &mut self.popup else { return };
        match k.code {
            KeyCode::Enter => {
                let (old, new) = (old.clone(), buf.clone());
                if new.is_empty() || new == old {
                    self.popup = Popup::None;
                    return;
                }
                if !valid_ident(&new) {
                    *err = "identifiers only — letters, digits, _  (no spaces)".into();
                    return; // keep the dialog open so it can be fixed
                }
                self.popup = Popup::None;
                let fname = self.name.clone();
                if ctx.bn.local_rename(&fname, &old, &new) {
                    self.apply_local_rename(ctx, &old, &new);
                    self.status = format!(" ✓ renamed {old} → {new}   (live · `bn save` to persist)");
                } else {
                    self.status = format!(" ✗ rename {old} → {new} failed");
                }
            }
            KeyCode::Esc => self.popup = Popup::None,
            KeyCode::Backspace => {
                buf.pop();
                err.clear();
            }
            KeyCode::Char(c) => {
                buf.push(c);
                err.clear();
            }
            _ => {}
        }
    }

    /// Reflect a completed local rename in place — no re-decompile. A local
    /// rename is just an identifier swap within this function, so we retext the
    /// tokens, move the locals entry, and rebuild spans. ~200ms → instant.
    fn apply_local_rename(&mut self, ctx: &Ctx, old: &str, new: &str) {
        for segs in &mut self.lines {
            for s in segs {
                if s.kind == Tok::Name && s.text == old {
                    s.text = new.to_string();
                }
            }
        }
        if let Some(ty) = self.locals.remove(old) {
            self.locals.insert(new.to_string(), ty);
        }
        let current = self.view.is_code().then(|| self.name.clone());
        self.spans = build_spans(&self.lines, ctx, &self.locals, current.as_deref());
    }

    /// Navigate to `target` (a function name or code address), pushing the nav
    /// stack. From an xrefs view, land on the *use* of the current symbol.
    fn goto_to(&mut self, ctx: &Ctx, target: String) {
        let landing = if self.view.is_code() { None } else { Some(self.name.clone()) };
        self.stack.push(Frame {
            name: self.name.clone(),
            view: self.view,
            top: self.top,
            cline: self.cline,
        });
        self.name = target;
        self.view = View::Decomp;
        self.load(ctx);
        self.top = 0;
        self.cline = 0;
        if let Some(sym) = landing {
            for (i, segs) in self.lines.iter().enumerate() {
                if segs.iter().any(|s| s.text == sym) {
                    self.cline = i;
                    self.top = i.saturating_sub(3);
                    break;
                }
            }
        }
    }

    /// Resolve a symbol or 0x-literal to an address — via the known map, else
    /// on demand through bn (which resolves internal data/func symbols too).
    fn resolve_addr(&self, ctx: &Ctx, sym: &str) -> Option<String> {
        if let Some(a) = ctx.addr_by_name.get(sym) {
            return Some(a.clone());
        }
        if sym.starts_with("0x") && sym.len() >= 4 {
            return Some(sym.to_string());
        }
        let out = ctx.bn.xrefs(sym);
        for line in out.lines() {
            if let Some(p) = line.find("xrefs to 0x") {
                let s = &line[p + "xrefs to ".len()..];
                let a: String = s.chars().take_while(|c| *c == 'x' || c.is_ascii_hexdigit()).collect();
                if a.starts_with("0x") && a.len() >= 4 {
                    return Some(a);
                }
            }
        }
        None
    }

    fn peek(&mut self, ctx: &Ctx, sym: &str) {
        match self.resolve_addr(ctx, sym) {
            Some(addr) => self.show_peek(ctx, sym, &addr),
            None => self.status = format!(" ✗ can't resolve {sym}"),
        }
    }

    /// `p`: peek the selected span, else the first resolvable symbol or
    /// 0x-address on the cursor line (so internal globals + literals are peekable).
    fn peek_on_line(&mut self, ctx: &Ctx) {
        if let Some(ci) = self.cur_span() {
            match self.spans[ci].kind {
                HotKind::Local => {
                    self.show_local(ci);
                    return;
                }
                HotKind::Str => {
                    self.peek_string(ctx, ci);
                    return;
                }
                _ => {}
            }
            let s = self.spans[ci].target.clone();
            self.peek(ctx, &s);
            return;
        }
        let toks: Vec<String> = self.lines[self.cline]
            .iter()
            .filter(|s| {
                s.kind == Tok::Name
                    || (s.kind == Tok::Num && s.text.starts_with("0x") && s.text.len() >= 6)
            })
            .map(|s| s.text.clone())
            .collect();
        for t in toks {
            if let Some(addr) = self.resolve_addr(ctx, &t) {
                self.show_peek(ctx, &t, &addr);
                return;
            }
        }
        self.status = " ✗ nothing to peek on this line".into();
    }

    fn show_peek(&mut self, ctx: &Ctx, sym: &str, addr: &str) {
        let dump = ctx.bn.read(addr, 256);
        self.popup = Popup::Peek {
            title: format!("peek {sym} @ {addr}"),
            lines: symbolize_dump(&dump, &ctx.name_by_addr),
            off: 0,
        };
    }

    fn open_xrefs(&mut self, ctx: &Ctx) {
        let tgt = match self.cur_span() {
            Some(ci) => match self.spans[ci].kind {
                HotKind::Local => {
                    self.status = " ✗ a local has no cross-references".into();
                    return;
                }
                HotKind::Str => {
                    let content = self.spans[ci].target.clone();
                    match ctx.strings().get(&content) {
                        Some(a) => a.clone(),
                        None => {
                            self.status = " ✗ couldn't resolve string".into();
                            return;
                        }
                    }
                }
                _ => self.spans[ci].target.clone(),
            },
            None => self.name.clone(),
        };
        self.stack.push(Frame {
            name: self.name.clone(),
            view: self.view,
            top: self.top,
            cline: self.cline,
        });
        self.name = tgt;
        self.view = View::Xrefs;
        self.load(ctx);
        self.top = 0;
        self.cline = 0;
    }

    fn back(&mut self, ctx: &Ctx) -> Exit {
        match self.stack.pop() {
            Some(f) => {
                self.name = f.name;
                self.view = f.view;
                self.load(ctx);
                self.top = f.top;
                self.cline = f.cline;
                Exit::Stay
            }
            None => Exit::Back,
        }
    }

    // ---- input ----
    pub fn on_key(&mut self, k: KeyEvent, ctx: &Ctx) -> Exit {
        self.status.clear();
        match &mut self.popup {
            Popup::Ask { .. } => {
                self.ask_key(k, ctx);
                return Exit::Stay;
            }
            Popup::Peek { .. } => {
                self.peek_key(k);
                return Exit::Stay;
            }
            Popup::Rename { .. } => {
                self.rename_key(k, ctx);
                return Exit::Stay;
            }
            Popup::None => {}
        }
        if self.search_input.is_some() {
            self.search_key(k);
            return Exit::Stay;
        }
        if self.vmode {
            return self.visual_key(k, ctx);
        }
        self.normal_key(k, ctx)
    }

    fn search_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => {
                if let Some(b) = self.search_input.take() {
                    self.search = b;
                }
                self.jump_match(1);
            }
            KeyCode::Esc => self.search_input = None,
            KeyCode::Backspace => {
                if let Some(b) = self.search_input.as_mut() {
                    b.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(b) = self.search_input.as_mut() {
                    b.push(c);
                }
            }
            _ => {}
        }
    }

    /// Jump the cursor to the next/prev line containing the search query.
    fn jump_match(&mut self, dir: i64) {
        if self.search.is_empty() || self.lines.is_empty() {
            return;
        }
        let q = self.search.to_lowercase();
        let n = self.lines.len();
        let hits: Vec<usize> = (0..n)
            .filter(|&i| self.line_text(i).to_lowercase().contains(&q))
            .collect();
        if hits.is_empty() {
            self.status = format!(" no match for '{}'", self.search);
            return;
        }
        let mut i = self.cline as i64;
        for _ in 0..n {
            i = (i + dir).rem_euclid(n as i64);
            let li = i as usize;
            if self.line_text(li).to_lowercase().contains(&q) {
                self.cline = li;
                self.top = li.saturating_sub(3);
                let rank = hits.iter().position(|&h| h == li).unwrap() + 1;
                self.status = format!(" /{}   match {}/{}", self.search, rank, hits.len());
                return;
            }
        }
    }

    fn normal_key(&mut self, k: KeyEvent, ctx: &Ctx) -> Exit {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let nlines = self.lines.len();
        let step_dn = |c: usize, n: usize| (c + n).min(nlines.saturating_sub(1)) * (n != 0) as usize + if n == 0 { c } else { 0 };
        let _ = step_dn;
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return Exit::Back,
            KeyCode::Char('j') | KeyCode::Down => self.move_c(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_c(-1),
            KeyCode::Char('d') if ctrl => self.move_c(10),
            KeyCode::Char('u') if ctrl => self.move_c(-10),
            KeyCode::PageDown => self.move_c(20),
            KeyCode::PageUp => self.move_c(-20),
            KeyCode::Char('G') => self.cline = nlines.saturating_sub(1),
            KeyCode::Char('H') | KeyCode::Home => self.cline = 0,
            KeyCode::Tab => self.next_symbol(1),
            KeyCode::BackTab => self.next_symbol(-1),
            KeyCode::Char('g') | KeyCode::Enter => self.act_primary(ctx),
            KeyCode::Char('p') => self.peek_on_line(ctx),
            KeyCode::Char('s') => {
                self.popup = Popup::Peek {
                    title: "sections  ·  r-x=exec  rw-=data  w+x flagged".into(),
                    lines: ctx.bn.sections(),
                    off: 0,
                };
            }
            KeyCode::Char('x') => self.open_xrefs(ctx),
            KeyCode::Char('r') => self.open_rename(),
            KeyCode::Char('i') => self.cycle_view(ctx, 1),
            KeyCode::Char('I') => self.cycle_view(ctx, -1),
            KeyCode::Char('b') => return self.back(ctx),
            KeyCode::Char('V') => {
                self.vmode = true;
                self.vanchor = self.cline;
            }
            KeyCode::Char('/') => self.search_input = Some(String::new()),
            KeyCode::Char('n') => self.jump_match(1),
            KeyCode::Char('N') => self.jump_match(-1),
            KeyCode::Char('?') => self.open_ask_line(ctx),
            _ => {}
        }
        Exit::Stay
    }

    fn visual_key(&mut self, k: KeyEvent, ctx: &Ctx) -> Exit {
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.vmode = false,
            KeyCode::Char('j') | KeyCode::Down => self.move_c(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_c(-1),
            KeyCode::Char('?') => self.open_ask_range(ctx),
            _ => {}
        }
        Exit::Stay
    }

    /// `i`/`I`: cycle the current function through Decomp → MLIL → Disasm (and
    /// back). From an xrefs view, enters Decomp. Reloads and resets to the top.
    fn cycle_view(&mut self, ctx: &Ctx, dir: i32) {
        let order = [View::Decomp, View::Mlil, View::Disasm];
        let next = match order.iter().position(|&v| v == self.view) {
            Some(cur) => order[(cur as i32 + dir).rem_euclid(3) as usize],
            None => View::Decomp, // from xrefs → decomp
        };
        self.view = next;
        self.load(ctx);
        self.top = 0;
        self.cline = 0;
        self.active = None;
        self.status = format!(" view: {}", self.view.label());
    }

    fn move_c(&mut self, d: i64) {
        let n = self.lines.len() as i64;
        self.cline = (self.cline as i64 + d).clamp(0, (n - 1).max(0)) as usize;
    }

    /// Tab/Shift-Tab: step through interactive spans one at a time (granular —
    /// multiple per line), wrapping. The render clamps `top` to keep it visible.
    fn next_symbol(&mut self, dir: i32) {
        if self.spans.is_empty() {
            return;
        }
        let n = self.spans.len() as i64;
        let i = match self.cur_span() {
            Some(c) => (c as i64 + dir as i64).rem_euclid(n) as usize,
            None if dir > 0 => self.spans.iter().position(|s| s.line > self.cline).unwrap_or(0),
            None => self
                .spans
                .iter()
                .rposition(|s| s.line < self.cline)
                .unwrap_or(self.spans.len() - 1),
        };
        self.active = Some(i);
        self.cline = self.spans[i].line;
    }

    /// Copy-pasteable locator: `-i/-t` selector + `function @ entry-address`.
    /// Single-line and `·`-delimited — `herdr pane run` submits on an embedded
    /// newline, so the whole payload must stay on one line.
    fn locator(&self, ctx: &Ctx) -> String {
        let mut s = String::from("[bn lens]");
        if ctx.instance_label != "(default)" && !ctx.instance_label.is_empty() {
            s.push_str(&format!(" -i {}", ctx.instance_label));
        }
        if !ctx.target.is_empty() {
            s.push_str(&format!(" -t {}", ctx.target));
        }
        // function @ entry address is the stable anchor (render line numbers
        // aren't); per-line addresses aren't cleanly available from HLIL.
        match ctx.addr_by_name.get(&self.name) {
            Some(a) => s.push_str(&format!(" · {} @ {}", self.name, a)),
            None => s.push_str(&format!(" · {}", self.name)),
        }
        s
    }

    fn open_ask_line(&mut self, ctx: &Ctx) {
        let mut snip = self.line_text(self.cline).trim().to_string();
        if snip.chars().count() > 140 {
            snip = snip.chars().take(139).collect::<String>() + "…";
        }
        self.popup = Popup::Ask {
            label: format!("{}:{}", self.name, self.cline + 1),
            preview: snip.clone(),
            prefix: format!(
                "{} · line {} · code: {} · [user] ",
                self.locator(ctx),
                self.cline + 1,
                snip
            ),
            buf: String::new(),
        };
    }

    fn open_ask_range(&mut self, ctx: &Ctx) {
        let lo = self.vanchor.min(self.cline);
        let hi = self.vanchor.max(self.cline);
        let mut block: Vec<String> = Vec::new();
        for i in lo..=hi {
            block.push(format!("{}: {}", i + 1, self.line_text(i).trim()));
        }
        let mut joined = block.join(" ⏎ ");
        if joined.chars().count() > 700 {
            joined = joined.chars().take(699).collect::<String>() + "…";
        }
        self.popup = Popup::Ask {
            label: format!("{}:{}-{}  ({} lines)", self.name, lo + 1, hi + 1, hi - lo + 1),
            preview: self.line_text(lo).trim().to_string(),
            prefix: format!(
                "{} · lines {}-{} · code: {} · [user] ",
                self.locator(ctx),
                lo + 1,
                hi + 1,
                joined
            ),
            buf: String::new(),
        };
        self.vmode = false;
    }

    fn ask_key(&mut self, k: KeyEvent, ctx: &Ctx) {
        let Popup::Ask { prefix, buf, .. } = &mut self.popup else { return };
        match k.code {
            KeyCode::Enter => {
                let msg = format!("{prefix}{buf}");
                // Fail closed: send only to the pane the lens was spawned from,
                // and only if it still hosts that *same* agent. Never guess a
                // recipient — the payload carries real target names.
                self.status = if ctx.agent_pane.is_empty() {
                    " ✗ no launching pane — relaunch bn lens from your agent's pane".into()
                } else {
                    match herdr::pane_agent(&ctx.herdr, &ctx.agent_pane) {
                        None => " ✗ launching pane has no agent".into(),
                        Some(a)
                            if !ctx.agent_session.is_empty()
                                && !a.session.is_empty()
                                && a.session != ctx.agent_session =>
                        {
                            " ✗ launching agent changed — not sending".into()
                        }
                        Some(_) if herdr::pane_run(&ctx.herdr, &ctx.agent_pane, &msg) => {
                            " ✓ sent to launching agent".into()
                        }
                        Some(_) => " ✗ send failed".into(),
                    }
                };
                self.popup = Popup::None;
            }
            KeyCode::Esc => self.popup = Popup::None,
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => buf.push(c),
            _ => {}
        }
    }

    fn peek_key(&mut self, k: KeyEvent) {
        let Popup::Peek { lines, off, .. } = &mut self.popup else { return };
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => self.popup = Popup::None,
            KeyCode::Char('j') | KeyCode::Down => *off = (*off + 1).min(lines.len().saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => *off = off.saturating_sub(1),
            KeyCode::PageDown => *off = (*off + 10).min(lines.len().saturating_sub(1)),
            KeyCode::PageUp => *off = off.saturating_sub(10),
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent, ctx: &Ctx) {
        if !matches!(self.popup, Popup::None) {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.move_c(-3),
            MouseEventKind::ScrollDown => self.move_c(3),
            MouseEventKind::Down(_) => {
                for &(x0, x1, y, idx) in &self.screen_tgts {
                    if m.row == y && m.column >= x0 && m.column < x1 {
                        self.active = Some(idx);
                        self.cline = self.spans[idx].line;
                        break;
                    }
                }
            }
            _ => {}
        }
        let _ = ctx;
    }

    // ---- render ----
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &Ctx) {
        let h = area.height as usize;
        let w = area.width as usize;
        let body_h = h.saturating_sub(3);
        self.cline = self.cline.min(self.lines.len().saturating_sub(1));
        if self.cline < self.top {
            self.top = self.cline;
        } else if self.cline >= self.top + body_h {
            self.top = self.cline + 1 - body_h;
        }
        let cand = self.cur_span();
        // a popup is a modal overlay: recede the backdrop (dim, no hotspot/cursor
        // styling) so nothing "spills" out around the box.
        let modal = !matches!(self.popup, Popup::None);
        // when a local is selected, all its occurrences highlight
        let active_local: Option<String> = cand.and_then(|i| {
            let s = &self.spans[i];
            (s.kind == HotKind::Local).then(|| s.target.clone())
        });
        let (vlo, vhi) = if self.vmode {
            (self.vanchor.min(self.cline), self.vanchor.max(self.cline))
        } else {
            (usize::MAX, 0)
        };

        // row 0 — the bar: what binary / arch / instance
        crate::ui::render_bar(buf, area.x, area.y, w, &crate::ui::crumbs(ctx));

        // row 1 — search prompt, status, visual banner, or the location line
        let addr = ctx.addr_by_name.get(&self.name).cloned().unwrap_or_default();
        let kind = self.view.label();
        let dim = Style::default().add_modifier(Modifier::DIM);
        if let Some(sq) = &self.search_input {
            buf.set_stringn(area.x, area.y + 1, format!(" /{sq}"), w,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        } else if !self.status.is_empty() {
            buf.set_stringn(area.x, area.y + 1, &self.status, w,
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD));
        } else if self.vmode {
            buf.set_stringn(area.x, area.y + 1,
                format!(" ● VISUAL · {} lines selected", vhi - vlo + 1), w,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        } else {
            let mut loc = Vec::new();
            if !addr.is_empty() {
                loc.push(Span::styled(format!(" {addr}"),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
                loc.push(Span::styled("  ", dim));
            }
            loc.push(Span::styled(format!("{kind}  "), Style::default().fg(Color::Blue)));
            loc.push(Span::styled(self.name.clone(),
                Style::default().add_modifier(Modifier::BOLD)));
            loc.push(Span::styled(format!("   {}/{}", self.cline + 1, self.lines.len()), dim));
            crate::ui::put_spans(buf, area.x, area.y + 1, w, &loc);
        }

        // row 2 — keys
        let hint = if self.search_input.is_some() {
            " type to find · Enter jump · Esc cancel"
        } else if self.vmode {
            " j/k extend · ? ask the agent · Esc cancel"
        } else {
            " j/k · Tab tok · i/I view · g goto · p peek · x xrefs · r rename · s sects · ? ask · / find · b back · q"
        };
        // footer bar (bottom row) — keys
        crate::ui::render_bar(
            buf,
            area.x,
            area.y + area.height.saturating_sub(1),
            w,
            &[Span::styled(hint, Style::default().add_modifier(Modifier::DIM))],
        );
        let search_lc = self.search.to_lowercase();

        self.screen_tgts.clear();
        let tgt_at: std::collections::HashMap<(usize, usize), usize> = self
            .spans
            .iter()
            .enumerate()
            .map(|(i, s)| ((s.line, s.col), i))
            .collect();

        let bottom = area.y + area.height.saturating_sub(1); // reserve footer row
        let right = area.x + area.width;
        let mut y = area.y + 2;
        let mut li = self.top;
        while y < bottom && li < self.lines.len() {
            let is_cur = li == self.cline;
            let selected = vlo <= li && li <= vhi;
            let is_match = !search_lc.is_empty()
                && self.line_text(li).to_lowercase().contains(&search_lc);
            let mark = if is_cur { "▸" } else if selected { "┃" } else if is_match { "◆" } else { " " };
            let gut = format!("{:>4}{}│ ", li + 1, mark);
            let gut_style = if modal {
                Style::default().fg(Color::Green).add_modifier(Modifier::DIM)
            } else if selected {
                Style::default().fg(Color::Green).add_modifier(Modifier::REVERSED)
            } else if is_cur {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else if is_match {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green).add_modifier(Modifier::DIM)
            };
            buf.set_stringn(area.x, y, gut, GUT as usize, gut_style);

            let cont_gut = format!("    {}↳ ", if selected { "┃" } else { " " });
            let cont_style = if selected {
                Style::default().fg(Color::Green).add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(Color::Green).add_modifier(Modifier::DIM)
            };

            let mut sx = area.x + GUT;
            let mut col = 0usize;
            'segs: for seg in &self.lines[li] {
                let tgt = tgt_at.get(&(li, col)).copied();
                let style = if modal {
                    Style::default().add_modifier(Modifier::DIM)
                } else if let Some(idx) = tgt {
                    let sp = &self.spans[idx];
                    let sibling = sp.kind == HotKind::Local
                        && active_local.as_deref() == Some(sp.target.as_str());
                    if Some(idx) == cand {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else if sibling {
                        // another occurrence of the selected local
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    } else {
                        match sp.kind {
                            HotKind::Func => Style::default().fg(theme::FUNC).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                            HotKind::Data => Style::default().fg(theme::DATA).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                            HotKind::Addr => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                            HotKind::Str => Style::default().fg(Color::Magenta).add_modifier(Modifier::UNDERLINED),
                            HotKind::Local => Style::default().fg(Color::Gray), // subtle until selected
                        }
                    }
                } else if matches!(self.view, View::Mlil | View::Disasm) {
                    // a muted asm/IL palette (the pseudo-C one rainbow-splits hex)
                    theme::asm_style(seg.kind)
                } else {
                    theme::tok_style(seg.kind)
                };
                let chars: Vec<char> = seg.text.chars().collect();
                let mut ci = 0;
                let mut first = true;
                while ci < chars.len() {
                    if sx >= right {
                        y += 1;
                        if y >= bottom {
                            break 'segs;
                        }
                        buf.set_stringn(area.x, y, &cont_gut, GUT as usize, cont_style);
                        sx = area.x + GUT;
                    }
                    let space = (right - sx) as usize;
                    let chunk: String = chars[ci..(ci + space).min(chars.len())].iter().collect();
                    let clen = chunk.chars().count();
                    buf.set_stringn(sx, y, &chunk, space, style);
                    if tgt.is_some() && first {
                        if let Some(idx) = tgt {
                            self.screen_tgts.push((sx, sx + clen as u16, y, idx));
                        }
                    }
                    first = false;
                    ci += clen;
                    sx += clen as u16;
                    col += clen;
                }
            }
            y += 1;
            li += 1;
        }

        self.render_popup(area, buf, ctx);
    }

    fn render_popup(&self, area: Rect, buf: &mut Buffer, ctx: &Ctx) {
        match &self.popup {
            Popup::None => {}
            Popup::Ask { label, preview, buf: input, .. } => {
                let bw = (area.width.saturating_sub(6)).clamp(46, 110);
                let bh = 8u16;
                let bx = area.x + (area.width.saturating_sub(bw)) / 2;
                let by = area.y + (area.height.saturating_sub(bh)) / 2;
                crate::ui::draw_box(buf, bx, by, bw, bh, "Ask the agent");
                // destination, so a mis-wired lens is caught before sending
                let dest = if ctx.agent_pane.is_empty() {
                    "→ (no launching pane — cannot send)".to_string()
                } else {
                    format!("→ sends to {}", ctx.agent_pane)
                };
                buf.set_stringn(bx + 2, by + 1, dest, (bw - 4) as usize, Style::default().fg(Color::Yellow));
                buf.set_stringn(bx + 2, by + 2, label, (bw - 4) as usize, Style::default().fg(Color::Cyan));
                buf.set_stringn(bx + 2, by + 3, preview, (bw - 4) as usize, Style::default().add_modifier(Modifier::DIM));
                buf.set_stringn(bx + 2, by + 5, format!("> {input}"), (bw - 4) as usize, Style::default());
                buf.set_stringn(bx + 2, by + bh - 1, " Enter send · Esc cancel ", (bw - 4) as usize, Style::default().add_modifier(Modifier::DIM));
            }
            Popup::Peek { title, lines, off } => {
                let bw = (area.width.saturating_sub(6)).clamp(50, 90);
                let bh = (area.height.saturating_sub(4)).clamp(8, 22);
                let bx = area.x + (area.width.saturating_sub(bw)) / 2;
                let by = area.y + (area.height.saturating_sub(bh)) / 2;
                crate::ui::draw_box(buf, bx, by, bw, bh, title);
                let view_h = (bh - 3) as usize;
                for (i, ln) in lines.iter().skip(*off).take(view_h).enumerate() {
                    buf.set_stringn(bx + 2, by + 1 + i as u16, ln, (bw - 4) as usize, Style::default().fg(Color::Yellow));
                }
                buf.set_stringn(bx + 2, by + bh - 1, " j/k scroll · q close ", (bw - 4) as usize, Style::default().add_modifier(Modifier::DIM));
            }
            Popup::Rename { old, buf: input, err } => {
                let bw = (area.width.saturating_sub(6)).clamp(46, 90);
                let bh = 7u16;
                let bx = area.x + (area.width.saturating_sub(bw)) / 2;
                let by = area.y + (area.height.saturating_sub(bh)) / 2;
                crate::ui::draw_box(buf, bx, by, bw, bh, "rename local  (live in the bn instance)");
                buf.set_stringn(bx + 2, by + 2, format!("{old}  →"), (bw - 4) as usize, Style::default().fg(Color::Cyan));
                buf.set_stringn(bx + 2, by + 3, format!("> {input}"), (bw - 4) as usize, Style::default());
                if !err.is_empty() {
                    buf.set_stringn(bx + 2, by + 4, format!("✗ {err}"), (bw - 4) as usize, Style::default().fg(Color::Red));
                }
                buf.set_stringn(bx + 2, by + bh - 1, " Enter rename · Esc cancel ", (bw - 4) as usize, Style::default().add_modifier(Modifier::DIM));
            }
        }
    }
}

/// Truncate `s` to `n` chars with an ellipsis (for popup titles).
fn ellipsize(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

/// Annotate a hex dump: any 8-byte little-endian value that matches a known
/// symbol address gets a `+off→name` note — so a function-pointer table reads
/// as names, not raw bytes.
fn symbolize_dump(dump: &str, rev: &std::collections::HashMap<String, String>) -> Vec<String> {
    dump.lines()
        .map(|line| {
            let after = line.splitn(2, ':').nth(1).unwrap_or(line);
            let bytes: Vec<u8> = after
                .split_whitespace()
                .take(16)
                .filter_map(|t| {
                    if t.len() == 2 {
                        u8::from_str_radix(t, 16).ok()
                    } else {
                        None
                    }
                })
                .collect();
            let mut ann = String::new();
            for off in [0usize, 8usize] {
                if bytes.len() >= off + 8 {
                    let mut v: u64 = 0;
                    for i in 0..8 {
                        v |= (bytes[off + i] as u64) << (8 * i);
                    }
                    if v != 0 {
                        if let Some(name) = rev.get(&format!("0x{v:x}")) {
                            if !ann.is_empty() {
                                ann.push_str(" ·");
                            }
                            ann.push_str(&format!(" +{off:#x}→{name}"));
                        }
                    }
                }
            }
            if ann.is_empty() {
                line.to_string()
            } else {
                format!("{line}  {ann}")
            }
        })
        .collect()
}

/// Is `t` a data global we can resolve/peek? Exports, imported data, any known
/// symbol, or an auto-named `data_<hex>` global.
fn is_data_symbol(ctx: &Ctx, t: &str) -> bool {
    ctx.data_names.contains(t)
        || ctx.addr_by_name.contains_key(t)
        || (ctx.import_names.contains(t) && !ctx.func_names.contains(t))
        || t.strip_prefix("data_")
            .is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Promote syntax segments to interactive spans: function names (goto), data
/// globals (peek), and 0x-addresses that land inside a mapped section (peek, or
/// goto if the section is executable). Constants/offsets (not in any section)
/// and locals/unknowns are left inert.
fn build_spans(
    lines: &[Line],
    ctx: &Ctx,
    locals: &std::collections::HashMap<String, String>,
    current: Option<&str>,
) -> Vec<Hotspot> {
    let mut out = Vec::new();
    for (li, segs) in lines.iter().enumerate() {
        let mut col = 0usize;
        for s in segs {
            let len = s.text.chars().count();
            match s.kind {
                Tok::Name if Some(s.text.as_str()) != current => {
                    if ctx.func_names.contains(&s.text) {
                        out.push(Hotspot { line: li, col, target: s.text.clone(), kind: HotKind::Func, code: true });
                    } else if is_data_symbol(ctx, &s.text) {
                        out.push(Hotspot { line: li, col, target: s.text.clone(), kind: HotKind::Data, code: false });
                    } else if locals.contains_key(&s.text) {
                        out.push(Hotspot { line: li, col, target: s.text.clone(), kind: HotKind::Local, code: false });
                    }
                }
                // 0x-address: Num in pseudo-C (tokenize_c), Type in the plain
                // tokenizer used for mlil/disasm/xrefs — accept either.
                Tok::Num | Tok::Type if s.text.starts_with("0x") => {
                    if let Some(v) = crate::ctx::parse_hex(&s.text) {
                        if let Some((_, _, _, exec)) = ctx.section_of(v) {
                            out.push(Hotspot { line: li, col, target: s.text.clone(), kind: HotKind::Addr, code: *exec });
                        }
                    }
                }
                Tok::Str => {
                    // store the content without the surrounding quotes (matches
                    // the strings map, which uses the same escape rendering)
                    if let Some(inner) = s.text.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
                        out.push(Hotspot { line: li, col, target: inner.to_string(), kind: HotKind::Str, code: false });
                    }
                }
                _ => {}
            }
            col += len;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::symbolize_dump;
    use std::collections::HashMap;

    #[test]
    fn symbolizes_pointer_in_dump() {
        // a little-endian pointer 0x402760 at offset +0x8
        let mut rev = HashMap::new();
        rev.insert("0x402760".to_string(), "handle_msg".to_string());
        let dump = "00415258: 00 00 00 00 00 00 00 00 60 27 40 00 00 00 00 00  ........";
        let out = symbolize_dump(dump, &rev);
        assert!(out[0].contains("+0x8→handle_msg"), "got: {}", out[0]);
    }

    #[test]
    fn leaves_nonmatching_lines_untouched() {
        let rev = HashMap::new();
        let dump = "00415258: 01 02 03 04 05 06 07 08 00 00 00 00 00 00 00 00  ........";
        let out = symbolize_dump(dump, &rev);
        assert_eq!(out[0], dump);
    }
}
