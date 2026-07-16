//! Viewer actions that load data or change navigation state.

use super::hotspots::{build_spans, ellipsize, symbolize_dump};
use super::{AnnTarget, Exit, Frame, HotKind, Popup, RenameScope, View, Viewer};
use crate::ctx::Ctx;
use crate::syntax::Tok;

impl Viewer {
    /// `g`/`Enter`: act on the selected span by kind — goto a function/code
    /// address, peek a data symbol/address.
    pub(super) fn act_primary(&mut self, ctx: &Ctx) {
        let Some(index) = self.cur_span() else {
            return;
        };
        let span = &self.spans[index];
        match span.kind {
            HotKind::Func => {
                let target = span.target.clone();
                // The signature name is a hotspot for x/r, but g on it is a
                // self-goto — nothing to navigate to.
                if self.view.is_code() && target == self.name {
                    self.status = format!(" {}   (x xrefs · r rename · ; comment)", self.name);
                } else {
                    self.goto_to(ctx, target);
                }
            }
            HotKind::Addr if span.code => {
                let target = span.target.clone();
                self.goto_to(ctx, target);
            }
            HotKind::Data | HotKind::Addr => {
                let target = span.target.clone();
                self.peek(ctx, &target);
            }
            HotKind::Local => self.show_local(index),
            HotKind::Str => self.peek_string(ctx, index),
        }
    }

    /// Peek a string literal: resolve its content to a `.rodata` address, then
    /// dump the bytes there.
    fn peek_string(&mut self, ctx: &Ctx, index: usize) {
        let content = self.spans[index].target.clone();
        match ctx.strings().get(&content) {
            Some(address) => {
                let address = address.clone();
                self.show_peek(ctx, &format!("\"{}\"", ellipsize(&content, 40)), &address);
            }
            None => self.status = " ✗ couldn't resolve string (not in .rodata)".into(),
        }
    }

    /// Show a local's type in the status line (its occurrences highlight in the
    /// body while it's selected).
    fn show_local(&mut self, index: usize) {
        let name = self.spans[index].target.clone();
        let ty = self
            .locals
            .get(&name)
            .cloned()
            .unwrap_or_else(|| "?".into());
        self.status = format!(" {name} : {ty}   (local · r to rename)");
    }

    /// `r`: rename. A selected Local → local rename; a selected Func hotspot →
    /// that function; otherwise the function currently in view.
    pub(super) fn open_rename(&mut self) {
        let (old, scope) = match self.cur_span() {
            Some(index) if self.spans[index].kind == HotKind::Local => {
                (self.spans[index].target.clone(), RenameScope::Local)
            }
            Some(index) if self.spans[index].kind == HotKind::Func => {
                (self.spans[index].target.clone(), RenameScope::Symbol)
            }
            _ if self.view.is_code() => (self.name.clone(), RenameScope::Symbol),
            _ => {
                self.status = " ✗ nothing renamable here".into();
                return;
            }
        };
        self.popup = Popup::Rename {
            old,
            buf: String::new(),
            err: String::new(),
            scope,
        };
    }

    /// `;`: comment. Targets a concrete address (selected Addr hotspot, or the
    /// address that leads a disasm/MLIL line), else the function doc comment.
    pub(super) fn open_comment(&mut self) {
        if !self.view.is_code() {
            self.status = " ✗ comments apply in a code view".into();
            return;
        }
        self.popup = Popup::Comment {
            target: self.ann_target(),
            buf: String::new(),
        };
    }

    /// `t`: bookmark/tag. Same target resolution as [`open_comment`].
    pub(super) fn open_tag(&mut self) {
        if !self.view.is_code() {
            self.status = " ✗ tags apply in a code view".into();
            return;
        }
        self.popup = Popup::Tag {
            target: self.ann_target(),
            buf: String::new(),
        };
    }

    /// Resolve where a comment/tag should land from the current selection/line.
    fn ann_target(&self) -> AnnTarget {
        if let Some(index) = self.cur_span() {
            if self.spans[index].kind == HotKind::Addr {
                return AnnTarget::Addr(self.spans[index].target.clone());
            }
        }
        if let Some(addr) = self.line_leading_addr() {
            return AnnTarget::Addr(addr);
        }
        AnnTarget::Func(self.name.clone())
    }

    /// The address that leads the current disasm/MLIL line, if any. Disasm emits
    /// bare hex runs (`0043274c  …`); normalize to `0x…`. Decompile has no
    /// per-line address, so this returns None there.
    fn line_leading_addr(&self) -> Option<String> {
        if !matches!(self.view, View::Disasm | View::Mlil) {
            return None;
        }
        let token = self.lines.get(self.cline)?.iter().find_map(|segment| {
            let text = segment.text.trim();
            (!text.is_empty()).then_some(text)
        })?;
        if let Some(hex) = token.strip_prefix("0x") {
            (hex.len() >= 4 && hex.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| token.to_string())
        } else if token.len() >= 6 && token.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(format!("0x{token}"))
        } else {
            None
        }
    }

    /// Reflect a completed local rename in place — no re-decompile. A local
    /// rename is just an identifier swap within this function, so we retext the
    /// tokens, move the locals entry, and rebuild spans.
    pub(super) fn apply_local_rename(&mut self, ctx: &Ctx, old: &str, new: &str) {
        for segments in &mut self.lines {
            for segment in segments {
                if segment.kind == Tok::Name && segment.text == old {
                    segment.text = new.to_string();
                }
            }
        }
        if let Some(ty) = self.locals.remove(old) {
            self.locals.insert(new.to_string(), ty);
        }
        self.stack_view.rename(old, new);
        self.spans = build_spans(&self.lines, ctx, &self.locals);
    }

    /// Navigate to `target` (a function name or code address), pushing the nav
    /// stack. From an xrefs view, land on the *use* of the current symbol.
    fn goto_to(&mut self, ctx: &Ctx, target: String) {
        let landing = if self.view.is_code() {
            None
        } else {
            Some(self.name.clone())
        };
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
        if let Some(symbol) = landing {
            for (index, segments) in self.lines.iter().enumerate() {
                if segments.iter().any(|segment| segment.text == symbol) {
                    self.cline = index;
                    self.top = index.saturating_sub(3);
                    break;
                }
            }
        }
    }

    /// Resolve a symbol or 0x-literal to an address — via the known map, else
    /// on demand through bn (which resolves internal data/func symbols too).
    fn resolve_addr(&self, ctx: &Ctx, symbol: &str) -> Option<String> {
        if let Some(address) = ctx.addr_by_name.get(symbol) {
            return Some(address.clone());
        }
        if symbol.starts_with("0x") && symbol.len() >= 4 {
            return Some(symbol.to_string());
        }
        let output = ctx.bn.xrefs(symbol);
        for line in output.lines() {
            if let Some(position) = line.find("xrefs to 0x") {
                let tail = &line[position + "xrefs to ".len()..];
                let address: String = tail
                    .chars()
                    .take_while(|ch| *ch == 'x' || ch.is_ascii_hexdigit())
                    .collect();
                if address.starts_with("0x") && address.len() >= 4 {
                    return Some(address);
                }
            }
        }
        None
    }

    fn peek(&mut self, ctx: &Ctx, symbol: &str) {
        match self.resolve_addr(ctx, symbol) {
            Some(address) => self.show_peek(ctx, symbol, &address),
            None => self.status = format!(" ✗ can't resolve {symbol}"),
        }
    }

    /// `p`: peek the selected span, else the first resolvable symbol or
    /// 0x-address on the cursor line (so internal globals + literals are peekable).
    pub(super) fn peek_on_line(&mut self, ctx: &Ctx) {
        if let Some(index) = self.cur_span() {
            match self.spans[index].kind {
                HotKind::Local => {
                    self.show_local(index);
                    return;
                }
                HotKind::Str => {
                    self.peek_string(ctx, index);
                    return;
                }
                _ => {}
            }
            let symbol = self.spans[index].target.clone();
            self.peek(ctx, &symbol);
            return;
        }
        let tokens: Vec<String> = self.lines[self.cline]
            .iter()
            .filter(|segment| {
                segment.kind == Tok::Name
                    || (segment.kind == Tok::Num
                        && segment.text.starts_with("0x")
                        && segment.text.len() >= 6)
            })
            .map(|segment| segment.text.clone())
            .collect();
        for token in tokens {
            if let Some(address) = self.resolve_addr(ctx, &token) {
                self.show_peek(ctx, &token, &address);
                return;
            }
        }
        self.status = " ✗ nothing to peek on this line".into();
    }

    fn show_peek(&mut self, ctx: &Ctx, symbol: &str, address: &str) {
        let dump = ctx.bn.read(address, 256);
        self.popup = Popup::Peek {
            title: format!("peek {symbol} @ {address}"),
            lines: symbolize_dump(&dump, &ctx.name_by_addr),
            off: 0,
        };
    }

    pub(super) fn open_xrefs(&mut self, ctx: &Ctx) {
        let target = match self.cur_span() {
            Some(index) => match self.spans[index].kind {
                HotKind::Local => {
                    self.status = " ✗ a local has no cross-references".into();
                    return;
                }
                HotKind::Str => {
                    let content = self.spans[index].target.clone();
                    match ctx.strings().get(&content) {
                        Some(address) => address.clone(),
                        None => {
                            self.status = " ✗ couldn't resolve string".into();
                            return;
                        }
                    }
                }
                _ => self.spans[index].target.clone(),
            },
            None => self.name.clone(),
        };
        self.stack.push(Frame {
            name: self.name.clone(),
            view: self.view,
            top: self.top,
            cline: self.cline,
        });
        self.name = target;
        self.view = View::Xrefs;
        self.load(ctx);
        self.top = 0;
        self.cline = 0;
    }

    pub(super) fn back(&mut self, ctx: &Ctx) -> Exit {
        match self.stack.pop() {
            Some(frame) => {
                self.name = frame.name;
                self.view = frame.view;
                self.load(ctx);
                self.top = frame.top;
                self.cline = frame.cline;
                Exit::Stay
            }
            None => Exit::Back,
        }
    }
}
