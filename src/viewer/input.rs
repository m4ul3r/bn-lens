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
                self.peek_key(key);
                return Exit::Stay;
            }
            Popup::Rename { .. } => return self.rename_key(key, ctx),
            Popup::Comment { .. } => {
                self.comment_key(key, ctx);
                return Exit::Stay;
            }
            Popup::Tag { .. } => {
                self.tag_key(key, ctx);
                return Exit::Stay;
            }
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

    fn comment_key(&mut self, key: KeyEvent, ctx: &Ctx) {
        let Popup::Comment { target, buf } = &mut self.popup else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let (target, text) = (target.clone(), buf.clone());
                self.popup = Popup::None;
                if text.is_empty() {
                    return;
                }
                let ok = match &target {
                    super::AnnTarget::Addr(addr) => ctx.bn.comment_set_addr(addr, &text),
                    super::AnnTarget::Func(func) => ctx.bn.comment_set_func(func, &text),
                };
                if ok {
                    self.status =
                        format!(" ✓ comment set {}   (`bn save` to persist)", target.label());
                    // Re-render so the note shows inline (no ctx change).
                    self.load(ctx);
                } else {
                    self.status = format!(" ✗ comment {} failed", target.label());
                }
            }
            KeyCode::Esc => self.popup = Popup::None,
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(ch) => buf.push(ch),
            _ => {}
        }
    }

    fn tag_key(&mut self, key: KeyEvent, ctx: &Ctx) {
        let Popup::Tag { target, buf } = &mut self.popup else {
            return;
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
            if self.line_text(candidate).to_lowercase().contains(&query) {
                self.cline = candidate;
                self.top = candidate.saturating_sub(3);
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
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Exit::Back,
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('d') if control => self.move_cursor(10),
            KeyCode::Char('u') if control => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(20),
            KeyCode::PageUp => self.move_cursor(-20),
            KeyCode::Char('G') => self.cline = line_count.saturating_sub(1),
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
                    hoff: 0,
                    focus: None,
                };
            }
            KeyCode::Char('S') => self.open_stack_view(),
            KeyCode::Char('x') => self.open_xrefs(ctx),
            KeyCode::Char('r') => self.open_rename(),
            KeyCode::Char(';') => self.open_comment(),
            KeyCode::Char('t') => self.open_tag(),
            KeyCode::Char('i') => self.cycle_il(ctx, 1),
            KeyCode::Char('I') => self.cycle_il(ctx, -1),
            KeyCode::Char('v') => self.toggle_cfg(ctx),
            KeyCode::Char(' ') if self.view == View::Cfg => self.toggle_cfg_graph(ctx),
            KeyCode::Char('b') => return self.back(ctx),
            KeyCode::Char('V') => {
                self.vmode = true;
                self.vanchor = self.cline;
            }
            KeyCode::Char('/') => self.search_input = Some(String::new()),
            KeyCode::Char('n') => self.jump_match(1),
            KeyCode::Char('N') => self.jump_match(-1),
            KeyCode::Char('a') => self.open_ask_line(ctx),
            _ => {}
        }
        Exit::Stay
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
    /// current IL linearly (no cycle).
    fn cycle_il(&mut self, ctx: &Ctx, direction: i32) {
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
        self.reload_view(ctx);
    }

    /// `v`: flip linear ⇄ CFG for the current function, keeping the IL
    /// rendering across the flip. From an xrefs view, enter the CFG.
    fn toggle_cfg(&mut self, ctx: &Ctx) {
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
        self.top = 0;
        self.cline = 0;
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
        self.top = 0;
        self.cline = 0;
        self.active = None;
    }

    /// Keys for the 2D CFG graph: hjkl move spatially, n/N step block index
    /// order, PgUp/PgDn scroll the always-on inspector, Enter reads the block
    /// as a list, Space toggles graph⇄list, i/I cycle the IL in place, v drops
    /// back to the linear view.
    fn cfg_graph_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Exit {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Exit::Back,
            KeyCode::Char('b') => return self.back(ctx),
            KeyCode::Char('h') | KeyCode::Left => self.cfg_move(CfgDir::Left),
            KeyCode::Char('l') | KeyCode::Right => self.cfg_move(CfgDir::Right),
            KeyCode::Char('j') | KeyCode::Down => self.cfg_move(CfgDir::Down),
            KeyCode::Char('k') | KeyCode::Up => self.cfg_move(CfgDir::Up),
            // Sequential walk (address/layout order) — distinct from spatial hjkl.
            KeyCode::Char('n') => self.cfg_step(1),
            KeyCode::Char('N') => self.cfg_step(-1),
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
    }

    /// Tab/Shift-Tab: step through interactive spans one at a time, wrapping.
    fn next_symbol(&mut self, direction: i32) {
        if self.spans.is_empty() {
            return;
        }
        let span_count = self.spans.len() as i64;
        let index = match self.cur_span() {
            Some(current) => (current as i64 + direction as i64).rem_euclid(span_count) as usize,
            None if direction > 0 => self
                .spans
                .iter()
                .position(|span| span.line > self.cline)
                .unwrap_or(0),
            None => self
                .spans
                .iter()
                .rposition(|span| span.line < self.cline)
                .unwrap_or(self.spans.len() - 1),
        };
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
        match ctx.addr_by_name.get(&self.name) {
            Some(address) => locator.push_str(&format!(" · {} @ {}", self.name, address)),
            None => locator.push_str(&format!(" · {}", self.name)),
        }
        locator
    }

    fn open_ask_line(&mut self, ctx: &Ctx) {
        let mut snippet = self.line_text(self.cline).trim().to_string();
        if snippet.chars().count() > 140 {
            snippet = snippet.chars().take(139).collect::<String>() + "…";
        }
        self.popup = Popup::Ask {
            label: format!("{}:{}", self.name, self.cline + 1),
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
                self.name,
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
                        Some(agent)
                            if !ctx.agent_session.is_empty()
                                && !agent.session.is_empty()
                                && agent.session != ctx.agent_session =>
                        {
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

    fn peek_key(&mut self, key: KeyEvent) {
        let Popup::Peek {
            lines, off, hoff, ..
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
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => self.popup = Popup::None,
            KeyCode::Char('j') | KeyCode::Down => {
                *off = (*off + 1).min(lines.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => *off = off.saturating_sub(1),
            KeyCode::PageDown => {
                *off = (*off + 10).min(lines.len().saturating_sub(1));
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
            MouseEventKind::ScrollUp => self.move_cursor(-3),
            MouseEventKind::ScrollDown => self.move_cursor(3),
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
