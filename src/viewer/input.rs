//! Keyboard and mouse handling for the code viewer.

use super::hotspots::valid_ident;
use super::stack::StackAction;
use super::{CfgDir, Exit, Popup, View, Viewer};
use crate::ctx::Ctx;
use crate::herdr;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

impl Viewer {
    pub fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Exit {
        self.status.clear();
        match &mut self.popup {
            Popup::Ask { .. } => {
                self.ask_key(key, ctx);
                return Exit::Stay;
            }
            Popup::Peek { .. } => {
                self.peek_key(key, ctx);
                return Exit::Stay;
            }
            Popup::Rename { .. } => return self.rename_key(key, ctx),
            Popup::Retype { .. } => return self.retype_key(key, ctx),
            Popup::Comment { .. } => return self.comment_key(key, ctx),
            Popup::Tag { .. } => return self.tag_key(key, ctx),
            Popup::None => {}
        }
        // The 2D CFG graph is navigated by block, not by line.
        if self.in_cfg_graph() {
            return self.cfg_graph_key(key, ctx);
        }
        if self.stack_view.is_open() {
            self.stack_key(key);
            return Exit::Stay;
        }
        if self.search_input.is_some() {
            self.search_key(key);
            return Exit::Stay;
        }
        if self.goto_input.is_some() {
            self.goto_key(key, ctx);
            return Exit::Stay;
        }
        if self.vmode {
            return self.visual_key(key, ctx);
        }
        self.normal_key(key, ctx)
    }

    fn rename_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Exit {
        let Popup::Rename {
            old,
            buf,
            err,
            scope,
        } = &mut self.popup
        else {
            return Exit::Stay;
        };
        match key.code {
            KeyCode::Enter => {
                let (old, new, scope) = (old.clone(), buf.clone(), *scope);
                if new.is_empty() || new == old {
                    self.popup = Popup::None;
                    return Exit::Stay;
                }
                if !valid_ident(&new) {
                    *err = "identifiers only — letters, digits, _  (no spaces)".into();
                    return Exit::Stay;
                }
                self.popup = Popup::None;
                return self.commit_rename(ctx, scope, &old, &new);
            }
            KeyCode::Esc => self.popup = Popup::None,
            KeyCode::Backspace => {
                buf.pop();
                err.clear();
            }
            KeyCode::Char(ch) => {
                buf.push(ch);
                err.clear();
            }
            _ => {}
        }
        Exit::Stay
    }

    /// Apply a validated rename. A local retexts in place (cheap); a function
    /// symbol needs a ctx rebuild, so we update `self.name` if it was the one in
    /// view and signal `Exit::Reload`.
    fn commit_rename(
        &mut self,
        ctx: &Ctx,
        scope: super::RenameScope,
        old: &str,
        new: &str,
    ) -> Exit {
        match scope {
            super::RenameScope::Local => {
                let function = self.name.clone();
                if ctx.bn.local_rename(&function, old, new) {
                    self.apply_local_rename(ctx, old, new);
                    self.status =
                        format!(" ✓ renamed {old} → {new}   (live · `bn save` to persist)");
                } else {
                    self.status = format!(" ✗ rename {old} → {new} failed");
                }
                Exit::Stay
            }
            super::RenameScope::Symbol => {
                if ctx.bn.symbol_rename(old, new) {
                    if self.name == old {
                        self.name = new.to_string();
                    }
                    self.status =
                        format!(" ✓ renamed fn {old} → {new}   (live · `bn save` to persist)");
                    Exit::Reload
                } else {
                    self.status = format!(" ✗ rename fn {old} → {new} failed");
                    Exit::Stay
                }
            }
        }
    }

    /// Max autocomplete suggestions shown/navigable in the retype composer.
    const RETYPE_SUGGESTIONS: usize = 6;

    fn retype_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Exit {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return self.retype_commit(ctx),
            // ^P: preview/validate the type without committing.
            KeyCode::Char('p') if control => self.retype_check(ctx),
            // Tab accepts the highlighted suggestion; ↑/↓ move the highlight.
            KeyCode::Tab => self.retype_accept_suggestion(),
            KeyCode::Down => self.retype_move_suggestion(1),
            KeyCode::Up => self.retype_move_suggestion(-1),
            KeyCode::Esc => self.popup = Popup::None,
            KeyCode::Backspace => {
                if let Popup::Retype {
                    buf, checked, sel, ..
                } = &mut self.popup
                {
                    buf.pop();
                    *checked = None;
                    *sel = 0;
                }
            }
            KeyCode::Char(ch) if !control => {
                if let Popup::Retype {
                    buf, checked, sel, ..
                } = &mut self.popup
                {
                    buf.push(ch);
                    *checked = None;
                    *sel = 0;
                }
            }
            _ => {}
        }
        Exit::Stay
    }

    /// Enter: validate the new type via `--preview` and, only if it parses,
    /// commit the retype. An invalid type shows the parser error and stays open.
    fn retype_commit(&mut self, ctx: &Ctx) -> Exit {
        let Popup::Retype {
            func,
            var,
            old_type,
            buf,
            ..
        } = &self.popup
        else {
            return Exit::Stay;
        };
        let (func, var, old, new) = (
            func.clone(),
            var.clone(),
            old_type.trim().to_string(),
            buf.trim().to_string(),
        );
        if new.is_empty() || new == old {
            self.popup = Popup::None;
            return Exit::Stay;
        }
        if let Err(message) = ctx.bn.local_retype_check(&func, &var, &new) {
            if let Popup::Retype { checked, .. } = &mut self.popup {
                *checked = Some(Err(message));
            }
            return Exit::Stay;
        }
        self.popup = Popup::None;
        if ctx.bn.local_retype(&func, &var, &new) {
            self.status = format!(" ✓ retyped {var} → {new}   (`bn save` to persist)");
            // Re-decompile so the new type (and any casts it removes) render.
            Exit::ReloadView
        } else {
            self.status = format!(" ✗ retype {var} → {new} failed");
            Exit::Stay
        }
    }

    /// `^P`: validate the current type without committing, storing the verdict.
    fn retype_check(&mut self, ctx: &Ctx) {
        let Popup::Retype { func, var, buf, .. } = &self.popup else {
            return;
        };
        let (func, var, new) = (func.clone(), var.clone(), buf.trim().to_string());
        let result = if new.is_empty() {
            Ok(())
        } else {
            ctx.bn.local_retype_check(&func, &var, &new)
        };
        if let Popup::Retype { checked, .. } = &mut self.popup {
            *checked = Some(result);
        }
    }

    fn retype_accept_suggestion(&mut self) {
        if let Popup::Retype {
            buf,
            types,
            sel,
            checked,
            ..
        } = &mut self.popup
        {
            let matches = super::hotspots::type_matches(types, buf, Self::RETYPE_SUGGESTIONS);
            if let Some(pick) = matches.get(*sel).or_else(|| matches.first()) {
                *buf = pick.clone();
                *checked = None;
                *sel = 0;
            }
        }
    }

    fn retype_move_suggestion(&mut self, delta: i32) {
        if let Popup::Retype {
            buf, types, sel, ..
        } = &mut self.popup
        {
            let count = super::hotspots::type_matches(types, buf, Self::RETYPE_SUGGESTIONS).len();
            if count > 0 {
                *sel = (*sel as i32 + delta).rem_euclid(count as i32) as usize;
            }
        }
    }

    fn comment_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Exit {
        let wrap = self.comment_wrap.get().max(1);
        let Popup::Comment {
            target,
            buf,
            cursor,
            existing,
        } = &mut self.popup
        else {
            return Exit::Stay;
        };
        let len = buf.chars().count();
        // Byte offset of char index `i` (== buf.len() at the end), for editing at
        // the caret without splitting a multi-byte char.
        let byte_at = |buf: &str, i: usize| {
            buf.char_indices()
                .nth(i)
                .map(|(b, _)| b)
                .unwrap_or(buf.len())
        };
        match key.code {
            KeyCode::Enter => {
                let (target, text, had) = (target.clone(), buf.clone(), *existing);
                self.popup = Popup::None;
                if text.is_empty() {
                    // Clearing an existing comment deletes it; an empty new
                    // comment is just discarded (nothing to write).
                    if !had {
                        return Exit::Stay;
                    }
                    let ok = match &target {
                        super::AnnTarget::Addr(addr) => ctx.bn.comment_delete_addr(addr),
                        super::AnnTarget::Func(func) => ctx.bn.comment_delete_func(func),
                    };
                    if ok {
                        self.status = format!(
                            " ✓ comment cleared {}   (`bn save` to persist)",
                            target.label()
                        );
                        return Exit::ReloadView;
                    }
                    self.status = format!(" ✗ comment {} clear failed", target.label());
                    return Exit::Stay;
                }
                let ok = match &target {
                    super::AnnTarget::Addr(addr) => ctx.bn.comment_set_addr(addr, &text),
                    super::AnnTarget::Func(func) => ctx.bn.comment_set_func(func, &text),
                };
                if ok {
                    self.status =
                        format!(" ✓ comment set {}   (`bn save` to persist)", target.label());
                    // Reload just this view so the comment renders inline. A
                    // comment doesn't touch ctx maps, and Marks rebuilds when
                    // next opened — so skip the full worker refresh + its stall.
                    return Exit::ReloadView;
                } else {
                    self.status = format!(" ✗ comment {} failed", target.label());
                }
            }
            KeyCode::Esc => self.popup = Popup::None,
            KeyCode::Left => *cursor = cursor.saturating_sub(1),
            KeyCode::Right => *cursor = (*cursor + 1).min(len),
            KeyCode::Home => *cursor = 0,
            KeyCode::End => *cursor = len,
            KeyCode::Up => *cursor = cursor.saturating_sub(wrap),
            KeyCode::Down => *cursor = (*cursor + wrap).min(len),
            KeyCode::Backspace => {
                if *cursor > 0 {
                    buf.remove(byte_at(buf, *cursor - 1));
                    *cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if *cursor < len {
                    buf.remove(byte_at(buf, *cursor));
                }
            }
            KeyCode::Char(ch) => {
                buf.insert(byte_at(buf, *cursor), ch);
                *cursor += 1;
            }
            _ => {}
        }
        Exit::Stay
    }

    fn tag_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Exit {
        let Popup::Tag { target, buf } = &mut self.popup else {
            return Exit::Stay;
        };
        match key.code {
            KeyCode::Enter => {
                let (target, note) = (target.clone(), buf.clone());
                self.popup = Popup::None;
                let ok = match &target {
                    super::AnnTarget::Addr(addr) => ctx.bn.tag_add_addr(addr, "Bookmarks", &note),
                    super::AnnTarget::Func(func) => ctx.bn.tag_add_func(func, "Bookmarks", &note),
                };
                if ok {
                    self.status =
                        format!(" ✓ bookmarked {}   (`bn save` to persist)", target.label());
                    // Live tag; no ctx change — reload just this view (Marks
                    // refreshes on its next open) instead of a full rebuild.
                    return Exit::ReloadView;
                } else {
                    self.status = format!(" ✗ tag {} failed", target.label());
                }
            }
            KeyCode::Esc => self.popup = Popup::None,
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(ch) => buf.push(ch),
            _ => {}
        }
        Exit::Stay
    }

    fn search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if let Some(input) = self.search_input.take() {
                    self.search = input;
                }
                self.jump_match(1);
            }
            KeyCode::Esc => self.search_input = None,
            KeyCode::Backspace => {
                if let Some(input) = self.search_input.as_mut() {
                    input.pop();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(input) = self.search_input.as_mut() {
                    input.push(ch);
                }
            }
            _ => {}
        }
    }

    /// Address/symbol goto (`:`): edit the buffer, commit on Enter. On commit the
    /// input is resolved and navigated in `goto_address`.
    fn goto_key(&mut self, key: KeyEvent, ctx: &Ctx) {
        match key.code {
            KeyCode::Enter => {
                if let Some(input) = self.goto_input.take() {
                    self.goto_address(ctx, &input);
                }
            }
            KeyCode::Esc => self.goto_input = None,
            KeyCode::Backspace => {
                if let Some(input) = self.goto_input.as_mut() {
                    input.pop();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(input) = self.goto_input.as_mut() {
                    input.push(ch);
                }
            }
            _ => {}
        }
    }

    /// Jump the cursor to the next/previous line containing the search query.
    fn jump_match(&mut self, direction: i64) {
        if self.search.is_empty() || self.lines.is_empty() {
            return;
        }
        let query = self.search.to_lowercase();
        let line_count = self.lines.len();
        let hits: Vec<usize> = (0..line_count)
            .filter(|&line| self.line_text(line).to_lowercase().contains(&query))
            .collect();
        if hits.is_empty() {
            self.status = format!(" no match for '{}'", self.search);
            return;
        }
        let mut line = self.cline as i64;
        for _ in 0..line_count {
            line = (line + direction).rem_euclid(line_count as i64);
            let candidate = line as usize;
            let original = self.line_text(candidate);
            let lower = original.to_lowercase();
            if lower.contains(&query) {
                self.cline = candidate;
                self.top = candidate.saturating_sub(3);
                // Select the *matched* token's hotspot (if the match lands on
                // one) so `g` follows the call you searched for instead of the
                // line's leftmost hotspot. The column is a char count into the
                // lowercased line; that equals the original-line column (which
                // spans/covering_span use) only when lowercasing preserves
                // length — true for ASCII, i.e. all of decompiled C outside
                // string bytes. If some char folded to a different length, skip
                // the selection rather than land it a column off. A match off
                // any hotspot clears the selection, falling back to the default.
                let aligned = original.chars().count() == lower.chars().count();
                self.active = aligned
                    .then(|| lower.find(&query))
                    .flatten()
                    .map(|byte| lower[..byte].chars().count())
                    .and_then(|col| super::hotspots::covering_span(&self.spans, candidate, col));
                let rank = hits
                    .iter()
                    .position(|&hit| hit == candidate)
                    .expect("matching line is present in precomputed hits")
                    + 1;
                self.status = format!(" /{}   match {}/{}", self.search, rank, hits.len());
                return;
            }
        }
    }

    fn normal_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Exit {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let line_count = self.lines.len();
        // Consume any pending `g` prefix up front: whatever this key is, the
        // latch is spent. `gg` (a second `g` while pending) jumps to the top;
        // any other key just proceeds, silently abandoning the prefix.
        let g_pending = std::mem::take(&mut self.g_pending);
        match key.code {
            // `q` leaves the viewer now; Esc backs out one layer at a time
            // (search highlight → nav history → list) — never skips history.
            KeyCode::Char('q') => return Exit::Back,
            KeyCode::Esc => return self.esc_back(ctx),
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('d') if control => self.move_cursor(10),
            KeyCode::Char('u') if control => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(20),
            KeyCode::PageUp => self.move_cursor(-20),
            KeyCode::Char('G') => {
                self.cline = line_count.saturating_sub(1);
                self.active = None;
            }
            KeyCode::Home => {
                self.cline = 0;
                self.active = None;
            }
            // Hotspot step (vim-ish word motion): functions, data, strings,
            // and every local (including v0_2-style temps — those get renamed).
            KeyCode::Char('w') | KeyCode::Tab => self.next_symbol(ctx, 1),
            KeyCode::Char('b') | KeyCode::BackTab => self.next_symbol(ctx, -1),
            // `W`/`B`: step only call/jump targets, skipping locals and data —
            // for following control flow without stopping on every temp.
            KeyCode::Char('W') => self.next_call(ctx, 1),
            KeyCode::Char('B') => self.next_call(ctx, -1),
            // Nav history (browser jumplist) — not in-view motion.
            KeyCode::Char('o') if control => return self.back(ctx),
            KeyCode::Char('f') if control => {
                self.go_forward(ctx);
                return Exit::Stay;
            }
            // `g` is a vim goto-prefix: `gg` (a second `g`) jumps to the top;
            // a lone `g` just arms the latch (consumed next key). Enter is now
            // the sole "go into / act on hotspot" key.
            KeyCode::Char('g') => {
                if g_pending {
                    self.cline = 0;
                    self.active = None;
                } else {
                    self.g_pending = true;
                }
            }
            KeyCode::Enter => self.act_primary(ctx),
            KeyCode::Char('p') => self.peek_on_line(ctx),
            KeyCode::Char('s') => {
                self.popup = Popup::Peek {
                    title: "sections  ·  r-x=exec  rw-=data  w+x flagged".into(),
                    lines: ctx.bn.sections(),
                    tokens: None,
                    goto: None,
                    off: 0,
                    hoff: 0,
                    focus: None,
                };
            }
            KeyCode::Char('S') => self.open_stack_view(),
            KeyCode::Char('x') => self.open_xrefs(ctx),
            KeyCode::Char('n') => self.open_rename(ctx),
            KeyCode::Char('y') => self.open_retype(ctx),
            KeyCode::Char(';') => self.open_comment(ctx),
            KeyCode::Char('t') => self.open_tag(),
            KeyCode::Char('i') => self.cycle_il(ctx, 1),
            KeyCode::Char('I') => self.cycle_il(ctx, -1),
            KeyCode::Char('v') => self.toggle_cfg(ctx),
            KeyCode::Char(' ') if self.view == View::Cfg => self.toggle_cfg_graph(ctx),
            KeyCode::Char('V') => {
                self.vmode = true;
                self.vanchor = self.cline;
            }
            KeyCode::Char('/') => self.search_input = Some(String::new()),
            KeyCode::Char(':') => self.goto_input = Some(String::new()),
            KeyCode::Char(']') => self.jump_match(1),
            KeyCode::Char('[') => self.jump_match(-1),
            KeyCode::Char('a') => self.open_ask_line(ctx),
            _ => {}
        }
        Exit::Stay
    }

    /// Esc in the normal path backs out exactly one layer. The modal layers
    /// (popup, stack panel, search input, visual mode) each consume Esc in
    /// their own handlers before this runs, so what's left is: clear an active
    /// search highlight, then pop the nav history, then leave to the list.
    fn esc_back(&mut self, ctx: &Ctx) -> Exit {
        if !self.search.is_empty() {
            self.search.clear();
            self.status = " search cleared".into();
            return Exit::Stay;
        }
        self.back(ctx)
    }

    fn open_stack_view(&mut self) {
        let preferred = self.cur_span().and_then(|index| {
            let span = &self.spans[index];
            (span.kind == super::HotKind::Local).then(|| span.target.clone())
        });
        if !self.stack_view.open(preferred.as_deref()) {
            self.status = " no stack-backed variables recovered for this view".into();
        }
    }

    fn stack_key(&mut self, key: KeyEvent) {
        match self.stack_view.on_key(key) {
            StackAction::None => {}
            StackAction::Close => self.stack_view.close(),
            StackAction::Jump(name) => {
                if let Some(index) = self
                    .spans
                    .iter()
                    .position(|span| span.kind == super::HotKind::Local && span.target == name)
                {
                    self.stack_view.close();
                    self.active = Some(index);
                    self.cline = self.spans[index].line;
                    self.top = self.cline.saturating_sub(3);
                } else {
                    self.stack_view.close();
                    self.status = format!(" {name} has no rendered use in this view");
                }
            }
            StackAction::Rename(old) => {
                self.stack_view.close();
                self.popup = Popup::Rename {
                    buf: String::new(),
                    old,
                    err: String::new(),
                    scope: super::RenameScope::Local,
                };
            }
        }
    }

    fn visual_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Exit {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.vmode = false,
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('a') => self.open_ask_range(ctx),
            _ => {}
        }
        Exit::Stay
    }

    /// `i`/`I`: cycle the IL rendering (Decomp → MLIL → Disasm and back). In a
    /// linear view the view itself changes; in the CFG view the graph stays up
    /// and re-fetches its blocks at the new IL. From an xrefs view, enter the
    /// current IL linearly (no cycle). The instruction under the cursor is kept
    /// across the switch — decompile→MLIL lands on the same statement's IL, not
    /// the top of the function.
    fn cycle_il(&mut self, ctx: &Ctx, direction: i32) {
        // The data view has no IL rendering; `i`/`I` are no-ops there.
        if self.view == View::Data {
            return;
        }
        // Remember where we are *before* changing the view, so the reload can
        // re-centre the new listing on the same address.
        let anchor = self.current_code_addr();
        if self.view != View::Xrefs {
            let order = [View::Decomp, View::Mlil, View::Disasm];
            let current = order
                .iter()
                .position(|&view| view == self.code_view)
                .unwrap_or(0);
            self.code_view =
                order[(current as i32 + direction).rem_euclid(order.len() as i32) as usize];
        }
        if self.view != View::Cfg {
            self.view = self.code_view;
        }
        // Drive the reload's re-centering through `focus_addr` (the same anchor
        // goto uses), then restore the prior focus so a *later* unrelated reload
        // — a comment/tag redraw — doesn't snap the view back to this spot.
        let prev_focus = self.focus_addr;
        if let Some(addr) = anchor {
            self.focus_addr = Some(addr);
        }
        self.reload_view(ctx);
        if anchor.is_some() {
            self.focus_addr = prev_focus;
        }
    }

    /// `v`: flip linear ⇄ CFG for the current function, keeping the IL
    /// rendering across the flip. From an xrefs view, enter the CFG.
    fn toggle_cfg(&mut self, ctx: &Ctx) {
        // No CFG for a data address; `v` is a no-op in the data view.
        if self.view == View::Data {
            return;
        }
        self.view = if self.view == View::Cfg {
            self.code_view
        } else {
            View::Cfg
        };
        self.reload_view(ctx);
    }

    /// Reload after an `i`/`v` view switch, resetting to the top. (`load_cfg`
    /// sets its own status for the CFG view.)
    fn reload_view(&mut self, ctx: &Ctx) {
        self.load(ctx);
        if self.focus_addr.is_none() {
            self.top = 0;
            self.cline = 0;
        }
        self.active = None;
        if !matches!(self.view, View::Cfg) {
            self.status = format!(" view: {}", self.view.label());
        }
    }

    /// Space (CFG view only): flip between the boxed graph layout and the flat
    /// block list, rebuilding in place.
    fn toggle_cfg_graph(&mut self, ctx: &Ctx) {
        self.cfg_graph = !self.cfg_graph;
        self.load(ctx);
        if self.focus_addr.is_none() {
            self.top = 0;
            self.cline = 0;
        }
        self.active = None;
    }

    /// Keys for the 2D CFG graph: hjkl move spatially, w/b and ]/[ step block
    /// index order, capitals `HJKL` pan the canvas and `z` recentres it on the
    /// selection, `e` toggles the block inspector, PgUp/PgDn scroll the inspector,
    /// Enter reads the block as a list, Space toggles graph⇄list, i/I cycle the IL
    /// in place, v drops back to the linear view. History is ^O/^F (same as linear).
    fn cfg_graph_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Exit {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // q leaves now; Esc pops the nav history. No `esc_back` ladder
            // here: the search highlight only renders in the linear view (the
            // graph has empty `self.lines`, so a match can't be shown), so
            // there's no highlight layer for Esc to clear first.
            KeyCode::Char('q') => return Exit::Back,
            KeyCode::Esc => return self.back(ctx),
            KeyCode::Char('o') if ctrl => return self.back(ctx),
            KeyCode::Char('f') if ctrl => {
                self.go_forward(ctx);
                return Exit::Stay;
            }
            KeyCode::Char('h') | KeyCode::Left => self.cfg_move(CfgDir::Left),
            KeyCode::Char('l') | KeyCode::Right => self.cfg_move(CfgDir::Right),
            KeyCode::Char('j') | KeyCode::Down => self.cfg_move(CfgDir::Down),
            KeyCode::Char('k') | KeyCode::Up => self.cfg_move(CfgDir::Up),
            // Capitals pan the whole canvas (keyboard twin of a mouse drag); `z`
            // recentres it on the selection, `e` toggles the block inspector.
            KeyCode::Char('H') => self.cfg_pan(-6, 0),
            KeyCode::Char('L') => self.cfg_pan(6, 0),
            KeyCode::Char('K') => self.cfg_pan(0, -3),
            KeyCode::Char('J') => self.cfg_pan(0, 3),
            KeyCode::Char('z') => self.cfg_recenter(),
            KeyCode::Char('e') => self.cfg_toggle_expand(),
            // Sequential walk (address/layout order) — distinct from spatial hjkl.
            // `w`/`b` match linear-view "next/prev unit" muscle memory.
            KeyCode::Char('w') | KeyCode::Char(']') => self.cfg_step(1),
            KeyCode::Char('b') | KeyCode::Char('[') => self.cfg_step(-1),
            // Panel scroll — does not steal hjkl from graph navigation.
            KeyCode::PageDown => {
                self.cfg_expand_scroll(10);
            }
            KeyCode::PageUp => {
                self.cfg_expand_scroll(-10);
            }
            KeyCode::Char('d') if ctrl => {
                self.cfg_expand_scroll(10);
            }
            KeyCode::Char('u') if ctrl => {
                self.cfg_expand_scroll(-10);
            }
            KeyCode::Enter | KeyCode::Char('g') => self.cfg_read_selected(ctx),
            KeyCode::Char(' ') => self.toggle_cfg_graph(ctx),
            KeyCode::Char('i') => self.cycle_il(ctx, 1),
            KeyCode::Char('I') => self.cycle_il(ctx, -1),
            KeyCode::Char('v') => self.toggle_cfg(ctx),
            _ => {}
        }
        Exit::Stay
    }

    fn move_cursor(&mut self, delta: i64) {
        let line_count = self.lines.len() as i64;
        self.cline = (self.cline as i64 + delta).clamp(0, (line_count - 1).max(0)) as usize;
        // Drop the Tab/click selection: a line move means the user is reading,
        // and a stale selection must not redirect a later rename/goto.
        self.active = None;
    }

    /// `w`/`b`/Tab/Shift-Tab: step interactive spans, wrapping. Non-code
    /// addresses and the viewed function's own name are skipped; **all locals**
    /// (including `v0_2` temps) stay in the ring so they can be renamed.
    fn next_symbol(&mut self, ctx: &Ctx, direction: i32) {
        if self.spans.is_empty() {
            return;
        }
        // `tab_stops` is non-empty whenever `spans` is (it falls back to every
        // span), so `step_stops` always lands on a stop.
        let stops = super::hotspots::tab_stops(&self.spans, ctx.display_name(&self.name));
        self.step_stops(stops, direction);
    }

    /// `W`/`B`: step only call/jump targets (functions and code addresses),
    /// skipping locals and data — for following control flow without stopping on
    /// every temp. Reports when the view has no calls (no ring to step).
    fn next_call(&mut self, ctx: &Ctx, direction: i32) {
        let stops = super::hotspots::call_stops(&self.spans, ctx.display_name(&self.name));
        if stops.is_empty() {
            self.status = " no calls or code targets in this view".into();
            return;
        }
        self.step_stops(stops, direction);
    }

    /// Shared stepping for `next_symbol`/`next_call`: ring-step from a
    /// *deliberate* selection (Tab/click, or a `/find` that selected a token) on
    /// the cursor line, else land on the nearest stop — preferring one on the
    /// cursor line (the `j`/`k`-then-step case). `stops` must be non-empty.
    fn step_stops(&mut self, stops: Vec<usize>, direction: i32) {
        if stops.is_empty() {
            return;
        }
        let active_pos = self
            .active
            .filter(|&index| self.spans.get(index).is_some_and(|s| s.line == self.cline))
            .and_then(|index| stops.iter().position(|&stop| stop == index));
        let stop_lines: Vec<usize> = stops.iter().map(|&index| self.spans[index].line).collect();
        let position = super::hotspots::next_stop(&stop_lines, active_pos, self.cline, direction);
        let index = stops[position];
        self.active = Some(index);
        self.cline = self.spans[index].line;
    }

    /// Copy-pasteable locator: `-i/-t` selector + `function @ entry-address`.
    /// Single-line and `·`-delimited because an embedded newline submits.
    fn locator(&self, ctx: &Ctx) -> String {
        let mut locator = String::from("[bn lens]");
        if ctx.instance_label != "(default)" && !ctx.instance_label.is_empty() {
            locator.push_str(&format!(" -i {}", ctx.instance_label));
        }
        if !ctx.target.is_empty() {
            locator.push_str(&format!(" -t {}", ctx.target));
        }
        let display_name = ctx.display_name(&self.name);
        match ctx.addr_by_name.get(&self.name).or(self.entry.as_ref()) {
            Some(address) => locator.push_str(&format!(" · {display_name} @ {address}")),
            None => locator.push_str(&format!(" · {display_name}")),
        }
        locator
    }

    fn open_ask_line(&mut self, ctx: &Ctx) {
        let mut snippet = self.line_text(self.cline).trim().to_string();
        if snippet.chars().count() > 140 {
            snippet = snippet.chars().take(139).collect::<String>() + "…";
        }
        self.popup = Popup::Ask {
            label: format!("{}:{}", ctx.display_name(&self.name), self.cline + 1),
            preview: snippet.clone(),
            prefix: format!(
                "{} · line {} · code: {} · [user] ",
                self.locator(ctx),
                self.cline + 1,
                snippet
            ),
            buf: String::new(),
        };
    }

    fn open_ask_range(&mut self, ctx: &Ctx) {
        let low = self.vanchor.min(self.cline);
        let high = self.vanchor.max(self.cline);
        let mut block = Vec::new();
        for line in low..=high {
            block.push(format!("{}: {}", line + 1, self.line_text(line).trim()));
        }
        let mut joined = block.join(" ⏎ ");
        if joined.chars().count() > 700 {
            joined = joined.chars().take(699).collect::<String>() + "…";
        }
        self.popup = Popup::Ask {
            label: format!(
                "{}:{}-{}  ({} lines)",
                ctx.display_name(&self.name),
                low + 1,
                high + 1,
                high - low + 1
            ),
            preview: self.line_text(low).trim().to_string(),
            prefix: format!(
                "{} · lines {}-{} · code: {} · [user] ",
                self.locator(ctx),
                low + 1,
                high + 1,
                joined
            ),
            buf: String::new(),
        };
        self.vmode = false;
    }

    fn ask_key(&mut self, key: KeyEvent, ctx: &Ctx) {
        let Popup::Ask { prefix, buf, .. } = &mut self.popup else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let message = format!("{prefix}{buf}");
                // Fail closed when the launching pane is absent or its agent changed.
                self.status = if ctx.agent_pane.is_empty() {
                    " ✗ no launching pane — relaunch bn lens from your agent's pane".into()
                } else {
                    match herdr::pane_agent(&ctx.herdr, &ctx.agent_pane) {
                        None => " ✗ launching pane has no agent".into(),
                        Some(agent) if !herdr::same_agent_session(&ctx.agent_session, &agent) => {
                            " ✗ launching agent changed — not sending".into()
                        }
                        Some(_) if herdr::pane_run(&ctx.herdr, &ctx.agent_pane, &message) => {
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
            KeyCode::Char(ch) => buf.push(ch),
            _ => {}
        }
    }

    fn peek_key(&mut self, key: KeyEvent, ctx: &Ctx) {
        let Popup::Peek {
            lines,
            off,
            hoff,
            goto,
            ..
        } = &mut self.popup
        else {
            return;
        };
        // Widest line, so horizontal pan can't scroll past the content.
        let max_h = lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = lines.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => self.popup = Popup::None,
            // `g` on a function/code peek jumps to that function (Enter still
            // just closes, matching the byte-dump/section peeks).
            KeyCode::Char('g') if !control => {
                if let Some(target) = goto.take() {
                    self.popup = Popup::None;
                    self.goto_to(ctx, target);
                }
            }
            // ^D/^U page (matching the main viewer's half-page step).
            KeyCode::Char('d') if control => *off = (*off + 10).min(last),
            KeyCode::Char('u') if control => *off = off.saturating_sub(10),
            KeyCode::Char('j') | KeyCode::Down => {
                *off = (*off + 1).min(last);
            }
            KeyCode::Char('k') | KeyCode::Up => *off = off.saturating_sub(1),
            KeyCode::PageDown => {
                *off = (*off + 10).min(last);
            }
            KeyCode::PageUp => *off = off.saturating_sub(10),
            KeyCode::Char('l') | KeyCode::Right => *hoff = (*hoff + 8).min(max_h),
            KeyCode::Char('h') | KeyCode::Left => *hoff = hoff.saturating_sub(8),
            KeyCode::Char('0') | KeyCode::Home => *hoff = 0,
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent, _ctx: &Ctx) {
        if !matches!(self.popup, Popup::None) {
            return;
        }
        if self.in_cfg_graph() {
            self.cfg_graph_mouse(mouse);
            return;
        }
        if self.stack_view.is_open() && self.stack_view.on_mouse(mouse) {
            return;
        }
        match mouse.kind {
            // Same step as PgUp/PgDn — a 3-line nudge felt like "changing the
            // highlight", not scrolling the decompile.
            MouseEventKind::ScrollUp => self.move_cursor(-20),
            MouseEventKind::ScrollDown => self.move_cursor(20),
            MouseEventKind::Down(_) => {
                for &(x0, x1, y, index) in &self.screen_tgts {
                    if mouse.row == y && mouse.column >= x0 && mouse.column < x1 {
                        self.active = Some(index);
                        self.cline = self.spans[index].line;
                        if self.spans[index].kind == super::HotKind::Local {
                            self.stack_view.select_name(&self.spans[index].target);
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    /// Mouse in the CFG graph. Drag pans the canvas freely (without moving the
    /// selection); a click (press+release with no drag) selects the box under the
    /// cursor; the wheel scrolls vertically. Panning/scrolling clears follow-mode
    /// so the viewport doesn't snap back to the selection.
    fn cfg_graph_mouse(&mut self, mouse: MouseEvent) {
        // Wheel over the always-on block inspector scrolls its instructions;
        // wheel over the canvas pans the graph.
        let over_expand = self.cfg_expand_hit(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp if over_expand => {
                self.cfg_expand_scroll(-3);
            }
            MouseEventKind::ScrollDown if over_expand => {
                self.cfg_expand_scroll(3);
            }
            MouseEventKind::ScrollUp => {
                if let Some(g) = self.cfg_graph_view.as_mut() {
                    g.top = g.top.saturating_sub(3);
                    g.follow = false;
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(g) = self.cfg_graph_view.as_mut() {
                    g.top += 3; // upper bound clamped in render
                    g.follow = false;
                }
            }
            MouseEventKind::ScrollLeft => {
                if let Some(g) = self.cfg_graph_view.as_mut() {
                    g.left = g.left.saturating_sub(3);
                    g.follow = false;
                }
            }
            MouseEventKind::ScrollRight => {
                if let Some(g) = self.cfg_graph_view.as_mut() {
                    g.left += 3;
                    g.follow = false;
                }
            }
            // Clicks / drags starting on the inspector don't pan or reselect.
            MouseEventKind::Down(_) if over_expand => {}
            MouseEventKind::Down(_) => {
                self.cfg_drag = Some((mouse.column, mouse.row));
                self.cfg_dragged = false;
            }
            MouseEventKind::Drag(_) => {
                if let Some((px, py)) = self.cfg_drag {
                    let dx = mouse.column as i64 - px as i64;
                    let dy = mouse.row as i64 - py as i64;
                    if let Some(g) = self.cfg_graph_view.as_mut() {
                        // Grab-and-drag: content moves with the cursor.
                        g.left = (g.left as i64 - dx).max(0) as usize;
                        g.top = (g.top as i64 - dy).max(0) as usize;
                        g.follow = false;
                    }
                    self.cfg_drag = Some((mouse.column, mouse.row));
                    self.cfg_dragged = true;
                }
            }
            MouseEventKind::Up(_) => {
                if !self.cfg_dragged && !over_expand {
                    // A click (no drag) selects the block under the cursor and
                    // refreshes the top-left inspector.
                    let hit = self
                        .cfg_hit
                        .iter()
                        .find(|&&(x0, x1, y0, y1, _)| {
                            mouse.column >= x0
                                && mouse.column < x1
                                && mouse.row >= y0
                                && mouse.row < y1
                        })
                        .map(|&(_, _, _, _, idx)| idx);
                    if let Some(idx) = hit {
                        self.cfg_select(idx);
                    }
                }
                self.cfg_drag = None;
                self.cfg_dragged = false;
            }
            _ => {}
        }
    }
}
