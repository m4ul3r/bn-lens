//! Keyboard and mouse handling for the code viewer.

use super::hotspots::valid_ident;
use super::stack::StackAction;
use super::{Exit, Popup, View, Viewer};
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
                    focus: None,
                };
            }
            KeyCode::Char('S') => self.open_stack_view(),
            KeyCode::Char('x') => self.open_xrefs(ctx),
            KeyCode::Char('r') => self.open_rename(),
            KeyCode::Char(';') => self.open_comment(),
            KeyCode::Char('t') => self.open_tag(),
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

    /// `i`/`I`: cycle the current function through Decomp → MLIL → Disasm (and
    /// back). From an xrefs view, enter Decomp. Reload and reset to the top.
    fn cycle_view(&mut self, ctx: &Ctx, direction: i32) {
        let order = [View::Decomp, View::Mlil, View::Disasm];
        let next = match order.iter().position(|&view| view == self.view) {
            Some(current) => order[(current as i32 + direction).rem_euclid(3) as usize],
            None => View::Decomp,
        };
        self.view = next;
        self.load(ctx);
        self.top = 0;
        self.cline = 0;
        self.active = None;
        self.status = format!(" view: {}", self.view.label());
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
        let Popup::Peek { lines, off, .. } = &mut self.popup else {
            return;
        };
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
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent, _ctx: &Ctx) {
        if !matches!(self.popup, Popup::None) {
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
}
