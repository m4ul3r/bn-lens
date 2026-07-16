//! Viewer actions that load data or change navigation state.

use super::hotspots::{build_spans, ellipsize, symbolize_dump};
use super::{Exit, Frame, HotKind, Popup, View, Viewer};
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
                self.goto_to(ctx, target);
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

    /// `r` on a selected local: open the rename dialog.
    pub(super) fn open_rename(&mut self) {
        let Some(index) = self.cur_span() else {
            return;
        };
        if self.spans[index].kind != HotKind::Local {
            self.status = " ✗ rename applies to a selected local".into();
            return;
        }
        self.popup = Popup::Rename {
            old: self.spans[index].target.clone(),
            buf: String::new(),
            err: String::new(),
        };
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
        let current = self.view.is_code().then(|| self.name.clone());
        self.spans = build_spans(&self.lines, ctx, &self.locals, current.as_deref());
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
