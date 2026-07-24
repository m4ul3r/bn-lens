//! Viewer actions that load data or change navigation state.

use super::hotspots::{build_spans, data_symbol_addr, ellipsize, symbolize_dump};
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
                // A within-function jump target (an MLIL `goto`/branch label) in
                // the linear views is already on screen: move the cursor to it in
                // place, preserving the view and function, instead of reloading.
                // Fall back to a real navigation when it's in another function.
                if matches!(self.view, View::Mlil | View::Disasm) {
                    if let Some(line) =
                        crate::ctx::parse_hex(&target).and_then(|addr| self.line_for_addr(addr))
                    {
                        self.push_nav(); // ^O returns to the pre-jump position
                        self.cline = line;
                        self.top = line.saturating_sub(3);
                        self.active = None;
                        return;
                    }
                }
                self.goto_to(ctx, target);
            }
            HotKind::Data | HotKind::Addr => {
                let target = span.target.clone();
                self.enter_data_view(ctx, &target);
            }
            HotKind::Local => self.show_local(index),
            HotKind::Str => self.peek_string(ctx, index),
        }
    }

    /// A one-line preview of the deliberately-selected hotspot (Tab/`w`/`W`/
    /// click, or a `/find` that landed on a token) resting on the cursor line,
    /// and what `g` will do to it. Rendered on the header row so the pending
    /// action — and *which* token is selected — is legible without leaning on the
    /// colour highlight. `None` while merely reading (plain `j`/`k` drops the
    /// selection), so the location readout shows instead.
    pub(super) fn hotspot_hint(&self, ctx: &Ctx) -> Option<String> {
        let index = self
            .active
            .filter(|&i| self.spans.get(i).is_some_and(|s| s.line == self.cline))?;
        let span = &self.spans[index];
        let hint = match span.kind {
            HotKind::Local => {
                let ty = self
                    .locals
                    .get(&span.target)
                    .map(String::as_str)
                    .unwrap_or("?");
                format!(" {} : {ty}   ·  local · n rename", span.target)
            }
            HotKind::Func if self.is_current_function(ctx, &span.target) => format!(
                " {}   ·  this fn · x xrefs · n rename · ; comment",
                ctx.display_name(&span.target)
            ),
            HotKind::Func => format!(" → {}   ·  g goto · x xrefs · n rename", span.target),
            HotKind::Addr if span.code => format!(" {}   ·  g goto · x xrefs", span.target),
            HotKind::Addr => format!(" {}   ·  g open data · p peek · x xrefs", span.target),
            HotKind::Data => format!(" {}   ·  g open data · p peek · x xrefs", span.target),
            HotKind::Str => format!(
                " \"{}\"   ·  g peek bytes · x xrefs",
                ellipsize(&span.target, 40)
            ),
        };
        Some(hint)
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

    /// `y`: retype the selected **local**. Opens a composer with type
    /// autocomplete (builtins + the target's declared types) that validates via
    /// `--preview` before committing — so a bad type never touches the instance.
    pub(super) fn open_retype(&mut self, ctx: &Ctx) {
        let Some(index) = self
            .cur_span()
            .filter(|&i| self.spans[i].kind == HotKind::Local && self.view.is_code())
        else {
            self.status = " ✗ y retypes a local — select one (w/b) first".into();
            return;
        };
        let var = self.spans[index].target.clone();
        let old_type = self.locals.get(&var).cloned().unwrap_or_default();
        // Autocomplete source: builtin primitives + every declared type, deduped.
        let mut types: Vec<String> = super::hotspots::BUILTIN_TYPES
            .iter()
            .map(|name| name.to_string())
            .collect();
        types.extend(ctx.bn.types_list().into_iter().map(|item| item.name));
        types.sort();
        types.dedup();
        self.popup = Popup::Retype {
            func: self.name.clone(),
            var,
            old_type,
            buf: String::new(),
            checked: None,
            types,
            sel: 0,
        };
    }

    /// `;`: comment. Targets a concrete address (selected Addr hotspot, or the
    /// address that leads a disasm/MLIL line), else the function doc comment.
    pub(super) fn open_comment(&mut self, ctx: &Ctx) {
        if !self.view.is_code() {
            self.status = " ✗ comments apply in a code view".into();
            return;
        }
        let (target, buf) = self.comment_edit_target(ctx);
        let cursor = buf.chars().count();
        let existing = !buf.is_empty();
        self.popup = Popup::Comment {
            target,
            buf,
            cursor,
            existing,
        };
    }

    /// Resolve where `;` comments *and* the current text to edit (empty for a
    /// new comment). A deliberately-selected Addr hotspot or a disasm/MLIL line's
    /// leading address always wins. Otherwise a *decompile* line with its own
    /// address gets an inline address comment — the case that lets `;` annotate a
    /// specific line rather than the whole function — **except** the entry line:
    /// bn tags the function's signature, its doc line, and the opening brace all
    /// with the entry address, so a line resolving to the entry is the function
    /// header, not a statement, and flows to [`func_comment_target`] (edit an
    /// existing doc in place; else target the entry address so a new note shows
    /// in Marks). Address-less lines (blank separators) fall there too.
    fn comment_edit_target(&self, ctx: &Ctx) -> (AnnTarget, String) {
        if let Some(addr) = self.explicit_addr() {
            let text = ctx.bn.comment_get_addr(&addr).unwrap_or_default();
            return (AnnTarget::Addr(addr), text);
        }
        let existing = ctx.bn.comment_get_func(&self.name);
        let entry = crate::ctx::parse_hex(&existing.entry_addr);
        if let Some(addr) = self.decomp_line_addr(ctx) {
            if entry.is_none() || crate::ctx::parse_hex(&addr) != entry {
                let text = ctx.bn.comment_get_addr(&addr).unwrap_or_default();
                return (AnnTarget::Addr(addr), text);
            }
        }
        func_comment_target(&self.name, existing)
    }

    /// The decompile cursor line's *own* address (its `--addresses` JSON entry),
    /// or `None` in another view or on a line carrying no address of its own (a
    /// declaration, brace, or blank line). Unlike [`current_code_addr`], it does
    /// **not** snap to a neighbouring line: an address-less line must fall
    /// through to the whole-function comment, not steal the next statement's
    /// address.
    fn decomp_line_addr(&self, ctx: &Ctx) -> Option<String> {
        if !matches!(self.view, View::Decomp) {
            return None;
        }
        let (_, _, text) = ctx.bn.decompile_json(&self.name)?;
        let dec = crate::decomp::dec_lines(&text);
        dec.get(self.cline)?.addr.map(|a| format!("0x{a:x}"))
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
        self.explicit_addr()
            .map(AnnTarget::Addr)
            .unwrap_or_else(|| AnnTarget::Func(self.name.clone()))
    }

    /// The explicit address `;`/`t` should annotate: a *deliberately* selected
    /// address hotspot (Tab/`w`/click), else the address leading the current
    /// disasm/MLIL line. Deliberate-only so the cursor merely resting on an
    /// address token inside a rendered comment doesn't hijack the target — a
    /// bare `;` then means "the function".
    fn explicit_addr(&self) -> Option<String> {
        if let Some(index) = self.active {
            if self
                .spans
                .get(index)
                .is_some_and(|span| span.kind == HotKind::Addr && span.line == self.cline)
            {
                return Some(self.spans[index].target.clone());
            }
        }
        self.line_leading_addr()
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

    /// The instruction address under the cursor in a linear code view, used to
    /// keep your place across an `i`/`I` IL switch (decompile→MLIL and back). A
    /// deliberately-selected address hotspot wins; otherwise MLIL/disasm read the
    /// line's leading address column, and decompile — which carries addresses
    /// only in its `--addresses` JSON, not in the rendered lines — re-maps the
    /// cursor line through that JSON, snapping to the nearest addressed line
    /// at/below the cursor (a declaration or brace line has no address of its own).
    pub(super) fn current_code_addr(&self, ctx: &Ctx) -> Option<u64> {
        match self.view {
            View::Mlil | View::Disasm => self
                .explicit_addr()
                .and_then(|addr| crate::ctx::parse_hex(&addr)),
            View::Decomp => {
                let (_, _, text) = ctx.bn.decompile_json(&self.name)?;
                let dec = crate::decomp::dec_lines(&text);
                dec.get(self.cline..)
                    .and_then(|rest| rest.iter().find_map(|line| line.addr))
                    .or_else(|| dec.iter().take(self.cline).rev().find_map(|line| line.addr))
            }
            _ => None,
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

    /// `g` on a data hotspot: resolve it and open the full **linear data view**
    /// (nav-history aware) — the inspectable counterpart to `p`'s quick popup.
    fn enter_data_view(&mut self, ctx: &Ctx, symbol: &str) {
        match self.resolve_addr(ctx, symbol) {
            Some(address) => self.open_data_view(ctx, &address),
            None => self.status = format!(" ✗ can't resolve {symbol}"),
        }
    }

    fn open_data_view(&mut self, ctx: &Ctx, address: &str) {
        let Some(addr) = crate::ctx::parse_hex(address) else {
            // Nothing to anchor a section on — fall back to the popup.
            self.show_data_map(ctx, address, address);
            return;
        };
        self.push_nav();
        self.focus_addr = Some(addr);
        self.name = address.to_string();
        // A data address has no owning function. Clear the entry anchor so it
        // can't leak the previously-viewed function's entry into the ask
        // locator or make `is_current_function` misfire in the data view.
        self.entry = None;
        self.view = View::Data;
        self.load(ctx);
    }

    /// Navigate to `target` (a function name or code address), pushing the nav
    /// stack. From an xrefs view, land on the *use* of the current symbol.
    /// The first linear-view line whose leading address token equals `addr`
    /// (the instruction-address column `cfg::flat` prints); `None` when the
    /// address isn't in the current listing (e.g. it belongs to another
    /// function). Used for in-place `goto`/branch jumps.
    fn line_for_addr(&self, addr: u64) -> Option<usize> {
        self.lines.iter().position(|segments| {
            segments.iter().find_map(|segment| {
                let text = segment.text.trim();
                let normalized = text.strip_prefix("0x").unwrap_or(text);
                (normalized.len() >= 6 && normalized.chars().all(|ch| ch.is_ascii_hexdigit()))
                    .then(|| u64::from_str_radix(normalized, 16).ok())
                    .flatten()
            }) == Some(addr)
        })
    }

    pub(super) fn goto_to(&mut self, ctx: &Ctx, target: String) {
        let landing = if self.view.is_code() {
            None
        } else {
            Some(self.name.clone())
        };
        // Preserve the reading level across a jump: following a call from MLIL
        // lands in the target's MLIL, disasm→disasm, decompile→decompile. From a
        // non-code view (xrefs/data) fall back to the last code view used.
        let keep = if self.view.is_code() {
            self.view
        } else {
            self.code_view
        };
        self.push_nav();
        self.focus_addr = target
            .starts_with("0x")
            .then(|| crate::ctx::parse_hex(&target))
            .flatten();
        self.name = target;
        self.view = keep;
        self.code_view = keep;
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

    /// `:` goto — resolve a typed address or symbol and navigate to it,
    /// preserving the current reading level. A code address lands in its
    /// containing function (the real name in the header, the cursor on the
    /// address); a data address opens the structured data view; anything
    /// unresolvable reports on the status line.
    pub(super) fn goto_address(&mut self, ctx: &Ctx, raw: &str) {
        let raw = raw.trim();
        if raw.is_empty() {
            return;
        }
        let addr_str = match self.resolve_goto_target(ctx, raw) {
            Some(addr) => addr,
            // Exact/hex resolution missed — try a unique symbol-name prefix so a
            // partial name still navigates (`vg_rev` → `vg_revert`).
            None => match match_name_prefix(ctx.addr_by_name.keys().map(String::as_str), raw) {
                NameMatch::Unique(name) => match ctx.addr_by_name.get(&name) {
                    Some(addr) => addr.clone(),
                    None => {
                        self.status = format!(" ✗ can't resolve '{raw}'");
                        return;
                    }
                },
                NameMatch::Ambiguous(count) => {
                    self.status = format!(" ✗ '{raw}' matches {count} names — be more specific");
                    return;
                }
                NameMatch::None => {
                    self.status = format!(" ✗ can't resolve '{raw}'");
                    return;
                }
            },
        };
        let Some(addr) = crate::ctx::parse_hex(&addr_str) else {
            self.status = format!(" ✗ can't resolve '{raw}'");
            return;
        };
        let id = format!("0x{addr:x}");
        if let Some((name, entry, _)) = ctx.bn.decompile_json(&id) {
            // A code address: land in the containing function, keeping the level.
            let resolved = ctx.name_by_addr.get(&entry).cloned().unwrap_or(name);
            let keep = if self.view.is_code() {
                self.view
            } else {
                self.code_view
            };
            self.push_nav();
            self.name = resolved;
            self.entry = (!entry.is_empty()).then_some(entry);
            self.focus_addr = Some(addr);
            self.view = keep;
            self.code_view = keep;
            self.load(ctx);
        } else {
            // Not inside a function — treat it as data (open_data_view pushes nav).
            self.open_data_view(ctx, &id);
        }
    }

    /// A short live hint for the `:` goto prompt: the unique name completion
    /// (`→ vg_revert`) or a match count for the in-progress query. Empty when the
    /// query is blank or an address (nothing to complete). Mirrors the same
    /// prefix resolution `goto_address` commits, so the hint never disagrees with
    /// what Enter does.
    pub(super) fn goto_hint(&self, ctx: &Ctx, query: &str) -> String {
        let q = query.trim();
        if q.is_empty() {
            return String::new();
        }
        // Address inputs (`0x…` or bare hex) resolve directly — no completion.
        let is_hex = q.starts_with("0x")
            || q.starts_with("0X")
            || (q.len() >= 3 && q.bytes().all(|b| b.is_ascii_hexdigit()));
        if is_hex {
            return String::new();
        }
        // Mirror `resolve_goto_target`: a case-sensitive exact name resolves as-is
        // (Enter jumps straight there), so don't show a prefix hint for it — else
        // a case-only collision (`foo` vs `FOO`) would read "2 matches" while
        // Enter succeeds on the exact `foo`.
        if ctx.addr_by_name.contains_key(q) {
            return String::new();
        }
        match match_name_prefix(ctx.addr_by_name.keys().map(String::as_str), q) {
            NameMatch::Unique(name) if name.eq_ignore_ascii_case(q) => String::new(),
            NameMatch::Unique(name) => format!("→ {name}"),
            NameMatch::Ambiguous(count) => format!("· {count} matches"),
            NameMatch::None => "· no match".to_string(),
        }
    }

    /// Turn a `:`-entered token into a canonical `0x…` address: a known symbol
    /// wins over a hex reading (so `add` stays a symbol), then `0x…`/bare-hex
    /// parse as hex, else fall back to the general symbol resolver. `None` when
    /// nothing resolves.
    fn resolve_goto_target(&self, ctx: &Ctx, raw: &str) -> Option<String> {
        if let Some(address) = ctx.addr_by_name.get(raw) {
            return Some(address.clone());
        }
        if raw.starts_with("0x") || raw.starts_with("0X") {
            return crate::ctx::parse_hex(raw).map(|addr| format!("0x{addr:x}"));
        }
        if raw.len() >= 3 && raw.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return u64::from_str_radix(raw, 16)
                .ok()
                .map(|addr| format!("0x{addr:x}"));
        }
        self.resolve_addr(ctx, raw)
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
        // An auto-named `data_<hex>` global encodes its own address; bn can't
        // resolve the synthetic name, so recover it directly instead of failing.
        if let Some(address) = data_symbol_addr(symbol) {
            return Some(address);
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
            Some(address) => self.show_data_map(ctx, symbol, &address),
            None => self.status = format!(" ✗ can't resolve {symbol}"),
        }
    }

    /// Peek a data address as a **structured field map**: the BN-typed data
    /// variables around it (address · name · type · value), pointers symbolized,
    /// section boundaries marked, centred on `address`. Falls back to a raw byte
    /// dump when BN exposes no typed data in range.
    fn show_data_map(&mut self, ctx: &Ctx, symbol: &str, address: &str) {
        let Some(addr) = crate::ctx::parse_hex(address) else {
            self.show_peek(ctx, symbol, address);
            return;
        };
        // A little context before, more after (fields tend to follow the one you
        // land on); clamp the low end to the containing section so the window
        // doesn't bleed into an unrelated prior section.
        let section = ctx.section_of(addr);
        let lo = match section {
            Some((start, _, _, _)) => addr.saturating_sub(0x40).max(*start),
            None => addr.saturating_sub(0x40),
        };
        let hi = addr + 0x200;
        let hint = section.map_or("", |(_, _, name, _)| name.as_str());
        let vars = ctx.bn.data_vars(&format!("0x{lo:x}"), &format!("0x{hi:x}"));
        if vars.is_empty() {
            self.show_peek(ctx, symbol, address);
            return;
        }
        let map = crate::datamap::render(&vars, Some(addr), hint);
        let title = if hint.is_empty() {
            format!("data · {symbol} @ {address}")
        } else {
            format!("data · {hint} @ {address}")
        };
        self.popup = Popup::Peek {
            title,
            off: map.focus.map_or(0, |index| index.saturating_sub(4)),
            focus: map.focus,
            lines: map.lines,
            tokens: None,
            goto: None,
            hoff: 0,
        };
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
                self.show_data_map(ctx, &token, &address);
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
            tokens: None,
            goto: None,
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
        // Tokenize the (marker-prefixed) body once so the peek gets the same
        // pseudo-C colours as the main viewer; the 2-char marker tokenizes as
        // plain, keeping alignment with `lines`.
        let tokens = crate::syntax::tokenize_c(&lines.join("\n"));
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
            tokens: Some(tokens),
            // `g` jumps to the peeked function by its entry address (already a
            // `0x…` selector from bn's JSON).
            goto: Some(entry.clone()),
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
                _ => {
                    let raw = self.spans[index].target.clone();
                    // A synthetic `data_<hex>` name isn't bn-resolvable; xref its
                    // encoded address instead. Named symbols/addresses pass through.
                    data_symbol_addr(&raw).unwrap_or(raw)
                }
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

/// Where a bare `;` (no address selected) lands on the current function, and
/// the existing text to edit. An existing `function_doc` keeps editing in place
/// (it's the note already rendered atop the signature — though it never lists
/// in Marks, since `bn comment list` doesn't enumerate `fn.comment`). Otherwise
/// a *new* comment targets the function's **entry address**: BN renders an
/// entry comment atop the function just the same, and an address-scoped comment
/// enumerates via `bn comment list`, so it shows in the Marks view (and to
/// every other client) instead of vanishing into `fn.comment`. The doc slot
/// remains only as a fallback when bn can't report an entry address.
fn func_comment_target(name: &str, existing: crate::bn::FuncComment) -> (AnnTarget, String) {
    if !existing.doc.trim().is_empty() {
        (AnnTarget::Func(name.to_string()), existing.doc)
    } else if !existing.entry_addr.is_empty() {
        (
            AnnTarget::Addr(existing.entry_addr),
            existing.entry_comment.unwrap_or_default(),
        )
    } else {
        (AnnTarget::Func(name.to_string()), String::new())
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

/// The result of resolving a `:` goto query against the known symbol names.
enum NameMatch {
    /// A single symbol — a case-insensitive exact hit, or a unique prefix.
    Unique(String),
    /// Several names share the prefix; too ambiguous to jump (report the count).
    Ambiguous(usize),
    None,
}

/// Resolve a goto query to a symbol name by case-insensitive exact match, else a
/// unique case-insensitive prefix — so `:` can reach `vg_revert` from `vg_rev`
/// without the full name. An exact (case-insensitive) hit always wins over a
/// longer prefix neighbour; an ambiguous prefix reports its count rather than
/// guessing a target.
fn match_name_prefix<'a>(names: impl Iterator<Item = &'a str>, query: &str) -> NameMatch {
    let query = query.trim();
    if query.is_empty() {
        return NameMatch::None;
    }
    let qb = query.as_bytes();
    let mut exact: Option<&str> = None;
    let mut exact_count = 0usize;
    let mut first_prefix: Option<&str> = None;
    let mut prefix_count = 0usize;
    for name in names {
        // ASCII-case-insensitive, allocation-free (this runs per keystroke for
        // the live goto hint). Byte-slicing is safe — `[u8]` has no boundary rule
        // and the length is guarded.
        if name.eq_ignore_ascii_case(query) {
            exact = Some(name);
            exact_count += 1;
        }
        let nb = name.as_bytes();
        if nb.len() >= qb.len() && nb[..qb.len()].eq_ignore_ascii_case(qb) {
            prefix_count += 1;
            first_prefix.get_or_insert(name);
        }
    }
    // Two names differing only by case are genuinely ambiguous — don't let map
    // iteration order pick one arbitrarily.
    if exact_count > 1 {
        return NameMatch::Ambiguous(exact_count);
    }
    if let Some(name) = exact {
        return NameMatch::Unique(name.to_string());
    }
    match prefix_count {
        0 => NameMatch::None,
        1 => NameMatch::Unique(first_prefix.unwrap().to_string()),
        n => NameMatch::Ambiguous(n),
    }
}

#[cfg(test)]
mod tests {
    use super::{func_comment_target, match_name_prefix, parse_xref_callsite, AnnTarget, NameMatch};
    use crate::bn::FuncComment;

    fn func_comment(doc: &str, entry_addr: &str, entry_comment: Option<&str>) -> FuncComment {
        FuncComment {
            doc: doc.into(),
            entry_addr: entry_addr.into(),
            entry_comment: entry_comment.map(str::to_string),
        }
    }

    #[test]
    fn bare_comment_on_a_function_targets_its_entry_address() {
        // No doc yet: a new function-level comment lands on the entry address so
        // it enumerates in `bn comment list` and lists in Marks — a `fn.comment`
        // doc would be invisible there (issue: function-doc comments missing
        // from the Marks view).
        assert_eq!(
            func_comment_target("parse_hdr", func_comment("", "0x401200", None)),
            (AnnTarget::Addr("0x401200".into()), String::new())
        );
        // A whitespace-only doc counts as absent.
        assert_eq!(
            func_comment_target("parse_hdr", func_comment("  \n", "0x401200", None)),
            (AnnTarget::Addr("0x401200".into()), String::new())
        );
    }

    #[test]
    fn existing_entry_comment_edits_in_place() {
        assert_eq!(
            func_comment_target(
                "copy_block",
                func_comment("", "0x4015e0", Some("bounds check lives here"))
            ),
            (
                AnnTarget::Addr("0x4015e0".into()),
                "bounds check lives here".into()
            )
        );
    }

    #[test]
    fn existing_function_doc_still_edits_the_doc() {
        // A pre-existing fn.comment (set before the entry-address change, or by
        // another client via `bn comment set --function`) keeps editing as a doc
        // rather than forking a second note at the entry.
        assert_eq!(
            func_comment_target(
                "vg_scan",
                func_comment("walks the volume table", "0x402000", None)
            ),
            (
                AnnTarget::Func("vg_scan".into()),
                "walks the volume table".into()
            )
        );
    }

    #[test]
    fn no_entry_address_falls_back_to_the_function_doc() {
        assert_eq!(
            func_comment_target("sub_403880", func_comment("", "", None)),
            (AnnTarget::Func("sub_403880".into()), String::new())
        );
    }

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

    fn matched(names: &[&str], query: &str) -> NameMatch {
        match_name_prefix(names.iter().copied(), query)
    }

    #[test]
    fn goto_prefix_resolves_a_unique_completion() {
        let names = ["vg_revert", "vg_reduce", "lvm_run", "main"];
        // unique prefix
        assert!(matches!(matched(&names, "vg_rev"), NameMatch::Unique(n) if n == "vg_revert"));
        assert!(matches!(matched(&names, "lvm_"), NameMatch::Unique(n) if n == "lvm_run"));
        // case-insensitive
        assert!(matches!(matched(&names, "MAIN"), NameMatch::Unique(n) if n == "main"));
    }

    #[test]
    fn goto_prefix_reports_ambiguity_and_misses() {
        let names = ["vg_revert", "vg_reduce", "lvm_run"];
        // shared prefix -> ambiguous with the count
        assert!(matches!(matched(&names, "vg_re"), NameMatch::Ambiguous(2)));
        assert!(matches!(matched(&names, "vg"), NameMatch::Ambiguous(2)));
        // no match / empty
        assert!(matches!(matched(&names, "zzz"), NameMatch::None));
        assert!(matches!(matched(&names, ""), NameMatch::None));
    }

    #[test]
    fn goto_prefix_prefers_exact_over_longer_neighbour() {
        // an exact (case-insensitive) name wins even when a longer name shares
        // the prefix, so `:foo` never becomes ambiguous against `foobar`.
        let names = ["foo", "foobar", "foobaz"];
        assert!(matches!(matched(&names, "foo"), NameMatch::Unique(n) if n == "foo"));
        assert!(matches!(matched(&names, "FOO"), NameMatch::Unique(n) if n == "foo"));
    }

    #[test]
    fn goto_prefix_case_only_collision_is_ambiguous() {
        // two symbols differing only by ASCII case are genuinely ambiguous —
        // never silently pick one by map order.
        let names = ["foo", "FOO"];
        assert!(matches!(matched(&names, "FoO"), NameMatch::Ambiguous(2)));
        assert!(matches!(matched(&names, "foo"), NameMatch::Ambiguous(2)));
        // trailing/leading whitespace in the query is ignored
        assert!(matches!(matched(&["main"], "  main  "), NameMatch::Unique(n) if n == "main"));
    }
}
