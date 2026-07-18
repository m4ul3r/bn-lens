//! Viewer actions that load data or change navigation state.

use super::hotspots::{build_spans, ellipsize, symbolize_dump};
use super::{AnnTarget, Exit, Frame, HotKind, Popup, RenameScope, View, Viewer};
use crate::ctx::Ctx;
use crate::syntax::Tok;

impl Viewer {
    fn is_current_function(&self, ctx: &Ctx, target: &str) -> bool {
        if target == self.name || target == ctx.display_name(&self.name) {
            return true;
        }
        let current = self
            .entry
            .as_ref()
            .or_else(|| ctx.addr_by_name.get(&self.name));
        current.is_some() && ctx.addr_by_name.get(target) == current
    }

    /// `g`/`Enter`: act on the selected span by kind — goto a function/code
    /// address, peek a data symbol/address.
    pub(super) fn act_primary(&mut self, ctx: &Ctx) {
        let Some(index) = self.cur_span() else {
            return;
        };
        let span = &self.spans[index];
        // In the CFG view, acting on an edge target (or a block address) jumps to
        // that block *in place* rather than re-decompiling the function.
        if self.view == View::Cfg && span.kind == HotKind::Addr {
            if let Some(&line) =
                crate::ctx::parse_hex(&span.target).and_then(|addr| self.cfg_index.get(&addr))
            {
                self.cline = line;
                self.top = line.saturating_sub(3);
                self.active = None;
                return;
            }
        }
        match span.kind {
            HotKind::Func => {
                let target = span.target.clone();
                // The signature name is a hotspot for x/r, but g on it is a
                // self-goto — nothing to navigate to.
                if self.view.is_code() && self.is_current_function(ctx, &target) {
                    self.status = format!(
                        " {}   (x xrefs · n rename · ; comment)",
                        ctx.display_name(&self.name)
                    );
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
        self.status = format!(" {name} : {ty}   (local · n to rename)");
    }

    /// `n`: rename. A selected Local → local rename; a selected Func hotspot
    /// naming *another* function → that symbol; otherwise (no selection, or a
    /// Data/Addr/Str selection) the function currently in view. Imported
    /// symbols are refused outright — renaming a PLT import retexts every call
    /// site and corrupts the decompile, so it must never happen just because
    /// Tab happened to rest on a call name.
    pub(super) fn open_rename(&mut self, ctx: &Ctx) {
        let (old, scope) = match self.cur_span() {
            Some(index) if self.spans[index].kind == HotKind::Local => {
                (self.spans[index].target.clone(), RenameScope::Local)
            }
            Some(index)
                if self.spans[index].kind == HotKind::Func
                    && !self.is_current_function(ctx, &self.spans[index].target) =>
            {
                (self.spans[index].target.clone(), RenameScope::Symbol)
            }
            _ if self.view.is_code() => (self.name.clone(), RenameScope::Symbol),
            _ => {
                self.status = " ✗ nothing renamable here".into();
                return;
            }
        };
        if scope == RenameScope::Symbol && ctx.import_names.contains(&old) {
            self.status =
                format!(" ✗ {old} is an import — not renaming (use bn directly if you mean it)");
            return;
        }
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
        // Retext only the segments the current spans classify as *this* local (by
        // (line, col) position), not every token that happens to share the name —
        // otherwise a same-named function/global (a shadow) or a struct field
        // would be rewritten too, diverging the display from real BN state.
        let targets: std::collections::HashSet<(usize, usize)> = self
            .spans
            .iter()
            .filter(|span| span.kind == HotKind::Local && span.target == old)
            .map(|span| (span.line, span.col))
            .collect();
        for (line_index, segments) in self.lines.iter_mut().enumerate() {
            let mut col = 0usize;
            for segment in segments.iter_mut() {
                // Advance by the *original* length so cols keep matching the
                // spans even after a same-line rename changes a token's width.
                let len = segment.text.chars().count();
                if segment.kind == Tok::Name
                    && segment.text == old
                    && targets.contains(&(line_index, col))
                {
                    segment.text = new.to_string();
                }
                col += len;
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
        self.push_nav();
        self.focus_addr = target
            .starts_with("0x")
            .then(|| crate::ctx::parse_hex(&target))
            .flatten();
        self.name = target;
        self.view = View::Decomp;
        self.code_view = View::Decomp;
        self.load(ctx);
        if self.focus_addr.is_none() {
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

    /// The first call *site* address on an xrefs caller line
    /// (`0xENTRY  fn  (N sites: 0xSITE, …)`) — the actual reference, distinct
    /// from the function-entry address that leads the line. `None` off such a
    /// line (e.g. the header rows).
    fn xref_callsite(&self) -> Option<String> {
        parse_xref_callsite(&self.line_text(self.cline))
    }

    /// `p`: peek the selected span, else the first resolvable symbol or
    /// 0x-address on the cursor line (so internal globals + literals are peekable).
    ///
    /// Code hotspots (a function name, or a `0x…` in an executable section — e.g.
    /// a callsite on the xrefs page) peek as **pseudo-C**: the containing
    /// function's decompile, centered on the use. Data peeks as a byte dump.
    pub(super) fn peek_on_line(&mut self, ctx: &Ctx) {
        // On an xrefs caller line the leading hotspot is the *function entry*,
        // so a default peek centers on the top of the caller (misleading — it
        // shows the prologue, not the reference). Prefer the actual call site
        // instead, unless the user Tab-selected a specific hotspot.
        if self.view == View::Xrefs && self.active.is_none() {
            if let Some(site) = self.xref_callsite() {
                let focus = crate::ctx::parse_hex(&site);
                self.peek_code(ctx, &site, focus, &format!("decomp @ {site}"));
                return;
            }
        }
        if let Some(index) = self.cur_span() {
            let kind = self.spans[index].kind;
            let target = self.spans[index].target.clone();
            let code = self.spans[index].code;
            match kind {
                HotKind::Local => self.show_local(index),
                HotKind::Str => self.peek_string(ctx, index),
                HotKind::Func => self.peek_code(ctx, &target, None, &format!("decomp · {target}")),
                HotKind::Addr if code => {
                    let focus = crate::ctx::parse_hex(&target);
                    self.peek_code(ctx, &target, focus, &format!("decomp @ {target}"));
                }
                HotKind::Addr | HotKind::Data => self.peek(ctx, &target),
            }
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
            hoff: 0,
            focus: None,
        };
    }

    /// Peek the decompilation of a code hotspot: decompile the containing
    /// function (JSON, so bn resolves an interior address and hands back the
    /// name), then open a scrollable pseudo-C popup — marking and centering the
    /// statement at `focus` when given. Powers `p` on a function or a code
    /// address (e.g. a callsite on the xrefs page), so a code ref reads as
    /// pseudo-C rather than raw bytes.
    fn peek_code(&mut self, ctx: &Ctx, identifier: &str, focus: Option<u64>, fallback: &str) {
        let Some((name, entry, text)) = ctx.bn.decompile_json(identifier) else {
            self.status = format!(" ✗ no decompile for {identifier}");
            return;
        };
        let dec = crate::decomp::dec_lines(&text);
        if dec.is_empty() {
            self.status = format!(" ✗ no decompile for {identifier}");
            return;
        }
        let addrs = crate::decomp::line_addrs(&dec);
        let focus_addr = focus.and_then(|site| crate::decomp::resolve_stmt_addr(&addrs, site));
        let mut lines = Vec::with_capacity(dec.len());
        let mut first_hit = None;
        for (index, line) in dec.iter().enumerate() {
            let hit = focus_addr.is_some() && line.addr == focus_addr;
            if hit && first_hit.is_none() {
                first_hit = Some(index);
            }
            let marker = if hit { "▸ " } else { "  " };
            lines.push(format!("{marker}{}", line.text));
        }
        // Center the focused statement with a few lines of lead-in context.
        let off = first_hit.map_or(0, |index| index.saturating_sub(4));
        let display_name = ctx
            .name_by_addr
            .get(&entry)
            .map(|name| ctx.display_name(name))
            .unwrap_or(name.as_str());
        let title = if display_name.is_empty() {
            fallback.to_string()
        } else {
            format!("decomp · {display_name}")
        };
        self.popup = Popup::Peek {
            title,
            lines,
            off,
            hoff: 0,
            focus: first_hit,
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
        self.push_nav();
        self.focus_addr = None;
        self.name = target;
        self.view = View::Xrefs;
        self.load(ctx);
        self.top = 0;
        self.cline = 0;
    }

    /// Snapshot the current location for the nav history.
    fn frame(&self) -> Frame {
        Frame {
            name: self.name.clone(),
            view: self.view,
            code_view: self.code_view,
            top: self.top,
            cline: self.cline,
            focus_addr: self.focus_addr,
        }
    }

    /// Record the current location before a *new* navigation (goto/xrefs).
    /// Like a browser, branching off invalidates the forward history.
    fn push_nav(&mut self) {
        let frame = self.frame();
        self.stack.push(frame);
        self.forward.clear();
    }

    fn restore(&mut self, ctx: &Ctx, frame: Frame) {
        self.name = frame.name;
        self.view = frame.view;
        self.code_view = frame.code_view;
        self.focus_addr = frame.focus_addr;
        self.load(ctx);
        self.top = frame.top;
        self.cline = frame.cline;
    }

    /// `^O` (and Esc at the bottom of its ladder): pop the nav stack, remembering
    /// the current view on the forward stack so `^F` can redo. With no history
    /// left, leave to the list.
    pub(super) fn back(&mut self, ctx: &Ctx) -> Exit {
        match self.stack.pop() {
            Some(frame) => {
                let here = self.frame();
                self.forward.push(here);
                self.restore(ctx, frame);
                Exit::Stay
            }
            None => Exit::Back,
        }
    }

    /// `^F`: forward — undo the latest `^O`/Esc history pop, pushing the current
    /// view back onto the nav stack so `^O`/`^F` walk the same trail both ways.
    pub(super) fn go_forward(&mut self, ctx: &Ctx) {
        match self.forward.pop() {
            Some(frame) => {
                let here = self.frame();
                self.stack.push(here);
                self.restore(ctx, frame);
            }
            None => self.status = " no forward history (^F redoes a ^O)".into(),
        }
    }
}

/// The first call-site address on an xrefs caller line
/// (`0xENTRY  fn  (N sites: 0xSITE, …)`) — the reference itself, not the
/// function-entry address that leads the line. `None` when the line has no
/// `site:` marker (header rows, blank lines).
fn parse_xref_callsite(line: &str) -> Option<String> {
    let (_, tail) = line.split_once("site")?;
    let start = tail.find("0x")?;
    let hex: String = tail[start..]
        .chars()
        .take_while(|c| *c == 'x' || c.is_ascii_hexdigit())
        .collect();
    (hex.len() >= 4).then_some(hex)
}

#[cfg(test)]
mod tests {
    use super::parse_xref_callsite;

    #[test]
    fn parses_the_first_site_not_the_entry() {
        // Single site: the address in parens, not the leading entry address.
        assert_eq!(
            parse_xref_callsite("  0x403340  main  (1 site: 0x4033a4)").as_deref(),
            Some("0x4033a4")
        );
        // Multiple sites: the first one.
        assert_eq!(
            parse_xref_callsite("  0x404af0  sub_404af0  (2 sites: 0x404f6c, 0x404f78)").as_deref(),
            Some("0x404f6c")
        );
    }

    #[test]
    fn no_callsite_on_non_ref_lines() {
        assert_eq!(
            parse_xref_callsite("xrefs to 0x402ae0 (1 code, 0 data)"),
            None
        );
        assert_eq!(
            parse_xref_callsite("code refs: 1 site across 1 function"),
            None
        );
        assert_eq!(parse_xref_callsite(""), None);
    }
}
