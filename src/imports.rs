//! The imports view: the binary's imported symbols — its attack surface. Known
//! dangerous sinks/sources are flagged so a vuln researcher can jump straight to
//! them. `Enter`/`x` cross-references an import (its callers land in the viewer);
//! `p` peeks *where it's called* with the pseudo-C at each callsite; `f` toggles
//! a sinks-only filter.

use crate::ctx::Ctx;
use crate::picker::Action;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Classify an import as a known-dangerous sink/source, returning a short
/// category label (else `None`). Normalizes fortified (`__*_chk`) and
/// underscore-prefixed variants, and catches obvious wrappers by substring on
/// high-signal tokens (so `tsk_sys_System`/`my_strcpy` still flag).
pub fn sink_category(name: &str) -> Option<&'static str> {
    let base = name.trim_start_matches('_');
    let base = base.strip_suffix("_chk").unwrap_or(base);
    let lower = base.to_ascii_lowercase();

    // (token, category) — exact match on the normalized name.
    const EXACT: &[(&str, &str)] = &[
        ("memcpy", "buffer"),
        ("memmove", "buffer"),
        ("strcpy", "buffer"),
        ("strcat", "buffer"),
        ("stpcpy", "buffer"),
        ("strncpy", "buffer"),
        ("strncat", "buffer"),
        ("bcopy", "buffer"),
        ("sprintf", "buffer"),
        ("vsprintf", "buffer"),
        ("gets", "buffer"),
        ("wcscpy", "buffer"),
        ("system", "command"),
        ("popen", "command"),
        ("execl", "command"),
        ("execlp", "command"),
        ("execle", "command"),
        ("execv", "command"),
        ("execvp", "command"),
        ("execvpe", "command"),
        ("execve", "command"),
        ("execveat", "command"),
        ("wordexp", "command"),
        ("printf", "format"),
        ("fprintf", "format"),
        ("snprintf", "format"),
        ("vsnprintf", "format"),
        ("dprintf", "format"),
        ("syslog", "format"),
        ("scanf", "source"),
        ("sscanf", "source"),
        ("fscanf", "source"),
        ("read", "source"),
        ("recv", "source"),
        ("recvfrom", "source"),
        ("fread", "source"),
        ("getenv", "source"),
    ];
    if let Some((_, cat)) = EXACT.iter().find(|(tok, _)| *tok == lower) {
        return Some(cat);
    }
    // Wrapper substrings unlikely to appear inside unrelated identifiers.
    const SUB: &[(&str, &str)] = &[
        ("strcpy", "buffer"),
        ("strcat", "buffer"),
        ("sprintf", "buffer"),
        ("memcpy", "buffer"),
        ("memmove", "buffer"),
        ("system", "command"),
        ("popen", "command"),
        ("execve", "command"),
        ("execvp", "command"),
        ("execlp", "command"),
    ];
    SUB.iter()
        .find(|(tok, _)| lower.contains(tok))
        .map(|(_, cat)| *cat)
}

/// A "high severity" category renders red; the rest yellow.
fn is_high(cat: &str) -> bool {
    matches!(cat, "buffer" | "command")
}

struct ImpItem {
    addr: String,
    name: String,
    sink: Option<&'static str>,
}

enum Mode {
    Normal,
    Search,
}

struct Usage {
    title: String,
    addr: String,
    lines: Vec<String>,
    off: usize,
}

pub struct ImportsList {
    items: Vec<ImpItem>,
    awidth: usize,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sinks_only: bool,
    sel: usize,
    top: usize,
    pending_g: bool,
    usage: Option<Usage>,
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok()
}

impl ImportsList {
    pub fn new(ctx: &Ctx) -> Self {
        let items = Self::build(ctx);
        let awidth = items
            .iter()
            .map(|it| it.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);
        ImportsList {
            items,
            awidth,
            filter: String::new(),
            prev_filter: String::new(),
            mode: Mode::Normal,
            sinks_only: false,
            sel: 0,
            top: 0,
            pending_g: false,
            usage: None,
        }
    }

    pub fn refresh(&mut self, ctx: &Ctx) {
        self.items = Self::build(ctx);
        self.awidth = self
            .items
            .iter()
            .map(|it| it.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);
    }

    /// Sinks first (by category severity), then the rest — both by address —
    /// so the attack surface sits at the top.
    fn build(ctx: &Ctx) -> Vec<ImpItem> {
        let mut items: Vec<ImpItem> = ctx
            .bn
            .imports_list()
            .into_iter()
            .map(|(addr, name)| {
                let sink = sink_category(&name);
                ImpItem { addr, name, sink }
            })
            .collect();
        items.sort_by(|a, b| {
            let rank = |it: &ImpItem| match it.sink {
                Some(c) if is_high(c) => 0,
                Some(_) => 1,
                None => 2,
            };
            rank(a).cmp(&rank(b)).then(
                parse_hex(&a.addr)
                    .unwrap_or(0)
                    .cmp(&parse_hex(&b.addr).unwrap_or(0)),
            )
        });
        items
    }

    pub fn is_searching(&self) -> bool {
        matches!(self.mode, Mode::Search)
    }

    pub fn popup_open(&self) -> bool {
        self.usage.is_some()
    }

    fn sink_count(&self) -> usize {
        self.items.iter().filter(|it| it.sink.is_some()).count()
    }

    fn filtered(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                if self.sinks_only && it.sink.is_none() {
                    return false;
                }
                f.is_empty()
                    || it.name.to_lowercase().contains(&f)
                    || it.addr.contains(&f)
                    || it.sink.is_some_and(|c| c.contains(&f))
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

    fn current(&self) -> Option<&ImpItem> {
        let rows = self.filtered();
        rows.get(self.sel).map(|&i| &self.items[i])
    }

    fn current_addr(&self) -> Option<String> {
        self.current().map(|it| it.addr.clone())
    }

    /// `p`: peek where the import is called — pseudo-C at each callsite.
    fn open_usage(&mut self, ctx: &Ctx) {
        let Some(item) = self.current() else { return };
        let (addr, name) = (item.addr.clone(), item.name.clone());
        self.usage = Some(Usage {
            title: format!("callers of {name}"),
            lines: crate::usage::report(ctx, &addr),
            addr,
            off: 0,
        });
    }

    fn usage_key(&mut self, k: KeyEvent) -> Action {
        let Some(usage) = &mut self.usage else {
            return Action::None;
        };
        let n = usage.lines.len();
        match k.code {
            KeyCode::Enter | KeyCode::Char('x') => {
                let addr = usage.addr.clone();
                self.usage = None;
                return Action::OpenXrefs(addr);
            }
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('p') => self.usage = None,
            KeyCode::Char('j') | KeyCode::Down => {
                usage.off = (usage.off + 1).min(n.saturating_sub(1))
            }
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
            KeyCode::Char('f') => {
                self.sinks_only = !self.sinks_only;
                self.sel = 0;
            }
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
            format!(
                "   imports  {}/{}  · {} sinks",
                rows.len(),
                self.items.len(),
                self.sink_count()
            ),
            Style::default().add_modifier(Modifier::DIM),
        ));
        crate::ui::render_bar(buf, x0, area.y, w, &bar);

        let (state, keys) = match self.mode {
            Mode::Search => (
                format!(" /{}", self.filter),
                " type · ↑↓ pick · Enter xrefs · Tab list · Esc cancel · ? help",
            ),
            Mode::Normal => (
                {
                    let base = if self.sinks_only {
                        " imports · sinks only"
                    } else {
                        " imports"
                    };
                    if self.filter.is_empty() {
                        base.to_string()
                    } else {
                        format!("{base} · filter: {}", self.filter)
                    }
                },
                " j/k move · / search · f sinks-only · p callers · Enter/x xrefs · m menu · q quit",
            ),
        };
        crate::ui::put_str(
            buf,
            x0,
            area.y + 1,
            state,
            w,
            Style::default().add_modifier(Modifier::DIM),
        );
        crate::ui::render_bar(
            buf,
            x0,
            area.y + area.height.saturating_sub(1),
            w,
            &[Span::styled(
                keys,
                Style::default().add_modifier(Modifier::DIM),
            )],
        );

        for (row, &i) in rows.iter().enumerate().skip(self.top).take(listh) {
            let y = area.y + 2 + (row - self.top) as u16;
            let it = &self.items[i];
            let is_sel = row == self.sel;
            let marker = match it.sink {
                Some(c) if is_high(c) => "⚠ ",
                Some(_) => "• ",
                None => "  ",
            };
            let tag = it.sink.map(|c| format!("  [{c}]")).unwrap_or_default();
            if is_sel {
                let text = format!(
                    "{marker}{:<aw$}  {}{tag}",
                    it.addr,
                    it.name,
                    aw = self.awidth
                );
                crate::ui::put_str(
                    buf,
                    x0,
                    y,
                    format!("{text:<w$}"),
                    w,
                    Style::default().add_modifier(Modifier::REVERSED),
                );
                continue;
            }
            let name_style = match it.sink {
                Some(c) if is_high(c) => {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                }
                Some(_) => Style::default().fg(Color::Yellow),
                None => Style::default().fg(crate::theme::NAME),
            };
            let mark_style = match it.sink {
                Some(c) if is_high(c) => Style::default().fg(Color::Red),
                Some(_) => Style::default().fg(Color::Yellow),
                None => Style::default(),
            };
            let spans = vec![
                Span::styled(marker.to_string(), mark_style),
                Span::styled(
                    format!("{:<aw$}", it.addr, aw = self.awidth),
                    Style::default()
                        .fg(crate::theme::ADDR)
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(format!("  {}", it.name), name_style),
                Span::styled(tag, Style::default().fg(Color::DarkGray)),
            ];
            crate::ui::put_spans(buf, x0, y, w, &spans);
        }

        self.render_usage(area, buf);
    }

    fn render_usage(&self, area: Rect, buf: &mut Buffer) {
        let Some(usage) = &self.usage else { return };
        let bw = area.width.saturating_sub(6).clamp(50, 100);
        let bh = area.height.saturating_sub(4).clamp(8, 30);
        let bx = area.x + (area.width.saturating_sub(bw)) / 2;
        let by = area.y + (area.height.saturating_sub(bh)) / 2;
        crate::ui::draw_box(buf, bx, by, bw, bh, &usage.title);
        let view_h = (bh as usize).saturating_sub(3);
        for (i, line) in usage.lines.iter().skip(usage.off).take(view_h).enumerate() {
            crate::ui::put_str(
                buf,
                bx + 2,
                by + 1 + i as u16,
                line,
                (bw - 4) as usize,
                Style::default().fg(Color::Yellow),
            );
        }
        crate::ui::put_str(
            buf,
            bx + 2,
            by + bh - 1,
            " j/k scroll · Enter/x opens full xrefs · p/q/Esc close ",
            (bw - 4) as usize,
            Style::default().add_modifier(Modifier::DIM),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::sink_category;

    #[test]
    fn classifies_libc_sinks_and_fortified_variants() {
        assert_eq!(sink_category("memcpy"), Some("buffer"));
        assert_eq!(sink_category("__memcpy_chk"), Some("buffer"));
        assert_eq!(sink_category("strcpy"), Some("buffer"));
        assert_eq!(sink_category("system"), Some("command"));
        assert_eq!(sink_category("execve"), Some("command"));
        assert_eq!(sink_category("snprintf"), Some("format"));
        assert_eq!(sink_category("recv"), Some("source"));
    }

    #[test]
    fn catches_wrappers_by_substring() {
        assert_eq!(sink_category("tsk_sys_System"), Some("command"));
        assert_eq!(sink_category("my_strcpy_safe"), Some("buffer"));
    }

    #[test]
    fn leaves_benign_imports_unflagged() {
        assert_eq!(sink_category("malloc"), None);
        assert_eq!(sink_category("targets"), None); // not a substring false-positive
        assert_eq!(sink_category("SysDbClientOpen"), None);
        assert_eq!(sink_category("pthread_mutex_lock"), None);
    }
}
