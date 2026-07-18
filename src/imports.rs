//! The imports view: the binary's imported symbols — its attack surface. Roles
//! come from bn's active taint-model presence catalog (not findings), with the
//! legacy name heuristic only as a compatibility fallback. `f` is a *real*
//! sinks-only filter; sources remain separately labeled/countable.

use crate::bn::ModelRoles;
use crate::ctx::Ctx;
use crate::picker::Action;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Classify an import as a known-dangerous sink/source, returning a short
/// category label (else `None`). Normalizes fortified (`__*_chk`), the glibc
/// `__isoc99_` scanf prefix, and underscore prefixes, then matches unambiguous
/// sink words at underscore-segment boundaries so wrappers like `tsk_sys_System`
/// / `my_strcpy` flag while `__system_property_get` / `filesystem` do not.
pub fn sink_category(name: &str) -> Option<&'static str> {
    let base = name.trim_start_matches('_');
    let base = base.strip_suffix("_chk").unwrap_or(base);
    let mut lower = base.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("isoc99_") {
        lower = rest.to_string(); // __isoc99_sscanf → sscanf
    }

    // (token, category) — exact match on the fully-normalized name.
    const EXACT: &[(&str, &str)] = &[
        ("memcpy", "buffer"),
        ("mempcpy", "buffer"),
        ("memmove", "buffer"),
        ("strcpy", "buffer"),
        ("strcat", "buffer"),
        ("stpcpy", "buffer"),
        ("stpncpy", "buffer"),
        ("strncpy", "buffer"),
        ("strncat", "buffer"),
        ("bcopy", "buffer"),
        ("sprintf", "buffer"),
        ("vsprintf", "buffer"),
        ("gets", "buffer"),
        ("alloca", "buffer"),
        ("wcscpy", "buffer"),
        ("wcsncpy", "buffer"),
        ("wcscat", "buffer"),
        ("wcsncat", "buffer"),
        ("wmemcpy", "buffer"),
        ("getwd", "buffer"),   // unbounded like gets — caller buffer, no size arg
        ("realpath", "buffer"), // resolves into caller buffer (PATH_MAX overflow)
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
        ("vprintf", "format"),
        ("vfprintf", "format"),
        ("vdprintf", "format"),
        ("syslog", "format"),
        ("wprintf", "format"),
        ("fwprintf", "format"),
        ("vwprintf", "format"),
        ("vfwprintf", "format"),
        ("swprintf", "format"),
        ("vswprintf", "format"),
        ("scanf", "source"),
        ("sscanf", "source"),
        ("fscanf", "source"),
        ("vscanf", "source"),
        ("vsscanf", "source"),
        ("vfscanf", "source"),
        ("read", "source"),
        ("recv", "source"),
        ("recvfrom", "source"),
        ("recvmsg", "source"),
        ("fread", "source"),
        ("fgets", "source"),
        ("readlink", "source"),   // fills buffer from fs, no NUL terminator
        ("readlinkat", "source"),
        ("getenv", "source"),
    ];
    if let Some((_, cat)) = EXACT.iter().find(|(tok, _)| *tok == lower) {
        return Some(cat);
    }

    // Wrapper detection at segment boundaries. `SEG` tokens are specific enough
    // that a match on *any* whole underscore-segment is a real wrapper
    // (`my_strcpy`, `bsp_MemCpy`, `svc_fgets`). Substring hits never count — only
    // whole segments. `read` is deliberately absent: it's a common benign segment
    // (`reg_read`, `spi_read`, `read_config`) — only the exact `read` import above
    // flags. `sscanf` is likewise absent: it parses an existing buffer, so it's a
    // taint *propagator*, not an input origin.
    const SEG: &[(&str, &str)] = &[
        ("strcpy", "buffer"),
        ("strncpy", "buffer"),
        ("strcat", "buffer"),
        ("strncat", "buffer"),
        ("sprintf", "buffer"),
        ("memcpy", "buffer"),
        ("mempcpy", "buffer"),
        ("memmove", "buffer"),
        ("popen", "command"),
        ("execl", "command"),
        ("execlp", "command"),
        ("execle", "command"),
        ("execv", "command"),
        ("execvp", "command"),
        ("execvpe", "command"),
        ("execve", "command"),
        // Input sources whose token is unambiguous as *any* segment.
        ("recvfrom", "source"),
        ("recvmsg", "source"),
        ("readlink", "source"),
        ("readlinkat", "source"),
        ("fgets", "source"),
        ("getenv", "source"),
    ];
    // `SEG_LAST` tokens *do* collide with benign non-final segments — `recv` in
    // `rtw_init_recv_priv`, `scanf` in the newlib `scanf_float` internals, `fread`
    // in flash-register names like `spi_mem_fread_qio`, `system` in
    // `system_property_get`. Trust them only as the *trailing* segment, which
    // still catches real wrappers (`net_recv`, `safe_fread`, `tsk_sys_System`).
    const SEG_LAST: &[(&str, &str)] = &[
        ("recv", "source"),
        ("scanf", "source"),
        ("fscanf", "source"),
        ("fread", "source"),
        ("system", "command"),
    ];
    let segs: Vec<&str> = lower.split('_').collect();
    for seg in &segs {
        if let Some((_, cat)) = SEG.iter().find(|(tok, _)| tok == seg) {
            return Some(cat);
        }
    }
    if let Some(last) = segs.last() {
        if let Some((_, cat)) = SEG_LAST.iter().find(|(tok, _)| tok == last) {
            return Some(cat);
        }
    }
    None
}

fn is_high(cat: &str) -> bool {
    cat.contains("overflow") || cat.contains("command") || cat == "buffer"
}

/// Resolve an import's roles, preferring the model catalog and supplementing a
/// per-import *miss* with the conservative `sink_category` heuristic. Returns
/// `(roles, heuristic)` where `heuristic` is true only when a present catalog
/// was silent about this import and the heuristic filled the gap — those rows
/// render as `hint:` candidates, visually distinct from catalog-authoritative
/// findings. In full heuristic-fallback mode (no catalog, `model_backed=false`)
/// every row is heuristic already and the footer discloses it, so rows are not
/// individually flagged as hints (avoids dimming the entire list).
fn resolve_roles(
    name: &str,
    raw_name: &str,
    models: &std::collections::HashMap<String, ModelRoles>,
    model_backed: bool,
) -> (ModelRoles, bool) {
    if let Some(roles) = models.get(raw_name).or_else(|| models.get(name)) {
        return (roles.clone(), false); // catalog is authoritative where present
    }
    let mut roles = ModelRoles::default();
    match sink_category(name) {
        Some("source") => roles.source = true,
        Some(category) => roles.sink_classes.push(category.to_string()),
        None => {}
    }
    let filled = roles.source || !roles.sink_classes.is_empty();
    (roles, model_backed && filled)
}

struct ImpItem {
    addr: String,
    name: String,
    raw_name: String,
    roles: ModelRoles,
    /// Roles came from the heuristic filling a catalog gap (see `resolve_roles`).
    heuristic: bool,
}

impl ImpItem {
    fn is_sink(&self) -> bool {
        !self.roles.sink_classes.is_empty()
    }

    fn is_source(&self) -> bool {
        self.roles.source
    }

    fn high_sink(&self) -> bool {
        self.roles.sink_classes.iter().any(|class| is_high(class))
    }

    fn role_label(&self) -> String {
        // Heuristic gap-fills render as `hint:` so their provenance is obvious;
        // catalog-backed roles keep the authoritative `sink:` prefix.
        let kind = if self.heuristic { "hint" } else { "sink" };
        let sink = (!self.roles.sink_classes.is_empty())
            .then(|| format!("{kind}:{}", self.roles.sink_classes.join(",")));
        match (self.roles.source, sink) {
            (true, Some(sink)) => format!("source+{sink}"),
            (true, None) if self.heuristic => "source?".into(),
            (true, None) => "source".into(),
            (false, Some(sink)) => sink,
            (false, None) if self.roles.propagator => "propagator".into(),
            (false, None) => String::new(),
        }
    }
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
    model_backed: bool,
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
        let (items, model_backed) = Self::build(ctx);
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
            model_backed,
            sel: 0,
            top: 0,
            pending_g: false,
            usage: None,
        }
    }

    pub fn refresh(&mut self, ctx: &Ctx) {
        let (items, model_backed) = Self::build(ctx);
        self.items = items;
        self.model_backed = model_backed;
        self.awidth = self
            .items
            .iter()
            .map(|it| it.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);
        // Keep the cursor valid if the rebuilt list shrank (else render can skip
        // past every row and blank the list until the next keypress).
        self.sel = self.sel.min(self.filtered().len().saturating_sub(1));
        self.top = self.top.min(self.sel);
    }

    /// Sinks first, then sources, then the rest — all by address.
    fn build(ctx: &Ctx) -> (Vec<ImpItem>, bool) {
        let imports = ctx.bn.imports_list();
        let (models, model_backed) = match ctx.bn.model_roles_present() {
            Ok(models) => (models, true),
            Err(_) => (std::collections::HashMap::new(), false),
        };
        let mut items: Vec<ImpItem> = imports
            .into_iter()
            .map(|import| {
                let (roles, heuristic) =
                    resolve_roles(&import.name, &import.raw_name, &models, model_backed);
                ImpItem {
                    addr: import.addr,
                    name: import.name,
                    raw_name: import.raw_name,
                    roles,
                    heuristic,
                }
            })
            .collect();
        items.sort_by(|a, b| {
            let rank = |it: &ImpItem| match (it.is_sink(), it.is_source()) {
                (true, _) if it.high_sink() => 0,
                (true, _) => 1,
                (false, true) => 2,
                _ => 3,
            };
            rank(a).cmp(&rank(b)).then(
                parse_hex(&a.addr)
                    .unwrap_or(0)
                    .cmp(&parse_hex(&b.addr).unwrap_or(0)),
            )
        });
        (items, model_backed)
    }

    pub fn is_searching(&self) -> bool {
        matches!(self.mode, Mode::Search)
    }

    pub fn popup_open(&self) -> bool {
        self.usage.is_some()
    }

    fn sink_count(&self) -> usize {
        self.items.iter().filter(|it| it.is_sink()).count()
    }

    fn source_count(&self) -> usize {
        self.items.iter().filter(|it| it.is_source()).count()
    }

    /// Sinks/sources whose roles came from the heuristic filling a catalog gap.
    fn hint_count(&self) -> usize {
        self.items.iter().filter(|it| it.heuristic).count()
    }

    fn filtered(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                if self.sinks_only && !it.is_sink() {
                    return false;
                }
                let roles = it.role_label().to_lowercase();
                f.is_empty()
                    || it.name.to_lowercase().contains(&f)
                    || it.raw_name.to_lowercase().contains(&f)
                    || it.addr.contains(&f)
                    || roles.contains(&f)
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

    /// `p`: peek where the import is called — exact asm plus approximate C.
    fn open_usage(&mut self, ctx: &Ctx) {
        let Some(item) = self.current() else { return };
        let (addr, name) = (item.addr.clone(), item.name.clone());
        self.usage = Some(Usage {
            title: format!("callers of {name}"),
            lines: crate::usage::report(ctx, &addr, &name),
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
            // q is the only quit. Esc backs out one filtering step at a time —
            // filter, then sinks-only — else returns to Symbols (never quits).
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.sel = 0;
                    self.top = 0;
                } else if self.sinks_only {
                    self.sinks_only = false;
                    self.sel = 0;
                    self.top = 0;
                } else {
                    return Action::Home;
                }
            }
            KeyCode::Char('i') => return Action::Switch,
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
            MouseEventKind::ScrollUp => self.move_sel(-20),
            MouseEventKind::ScrollDown => self.move_sel(20),
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
        let hints = self.hint_count();
        bar.push(Span::styled(
            format!(
                "   imports  {}/{}  · {} sinks · {} sources{}{}",
                rows.len(),
                self.items.len(),
                self.sink_count(),
                self.source_count(),
                if hints > 0 {
                    format!(" · {hints} hint")
                } else {
                    String::new()
                },
                if self.model_backed {
                    " · model catalog"
                } else {
                    " · heuristic fallback"
                }
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
                if self.sinks_only && self.filter.is_empty() {
                    " sinks only".to_string()
                } else if self.sinks_only {
                    format!(" sinks only · filter: {}", self.filter)
                } else if self.filter.is_empty() {
                    if self.model_backed {
                        " presence catalog only · NOT vulnerability findings".to_string()
                    } else {
                        " model catalog unavailable · heuristic labels only · NOT findings"
                            .to_string()
                    }
                } else {
                    format!(" filter: {}", self.filter)
                },
                " j/k move · / search · f actual-sinks · p callers · Enter/x xrefs · m menu · v next list · i switch · q quit",
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
            let marker = if it.heuristic {
                "? " // heuristic gap-fill — a candidate, not a catalog finding
            } else {
                match (it.is_sink(), it.is_source()) {
                    (true, true) => "◆ ",
                    (true, false) => "⚠ ",
                    (false, true) => "← ",
                    (false, false) => "  ",
                }
            };
            let label = it.role_label();
            let tag = if label.is_empty() {
                String::new()
            } else {
                format!("  [{label}]")
            };
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
            let name_style = match (it.is_sink(), it.is_source()) {
                (true, _) if it.high_sink() => {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                }
                (true, true) => Style::default().fg(Color::Magenta),
                (true, false) => Style::default().fg(Color::Yellow),
                (false, true) => Style::default().fg(Color::Cyan),
                (false, false) => Style::default().fg(crate::theme::NAME),
            };
            let mark_style = match (it.is_sink(), it.is_source()) {
                (true, _) if it.high_sink() => Style::default().fg(Color::Red),
                (true, true) => Style::default().fg(Color::Magenta),
                (true, false) => Style::default().fg(Color::Yellow),
                (false, true) => Style::default().fg(Color::Cyan),
                (false, false) => Style::default(),
            };
            // Heuristic gap-fills recede — dimmed so they never masquerade as
            // catalog-authoritative findings.
            let (name_style, mark_style) = if it.heuristic {
                (
                    name_style.add_modifier(Modifier::DIM),
                    mark_style.add_modifier(Modifier::DIM),
                )
            } else {
                (name_style, mark_style)
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
    use super::{resolve_roles, sink_category};
    use crate::bn::ModelRoles;
    use std::collections::HashMap;

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
    fn normalizes_glibc_isoc99_and_extra_families() {
        // glibc scanf sources import as __isoc99_*
        assert_eq!(sink_category("__isoc99_sscanf"), Some("source"));
        assert_eq!(sink_category("__isoc99_scanf"), Some("source"));
        // previously-missed families
        assert_eq!(sink_category("mempcpy"), Some("buffer"));
        assert_eq!(sink_category("__mempcpy_chk"), Some("buffer"));
        assert_eq!(sink_category("wcsncpy"), Some("buffer"));
        assert_eq!(sink_category("alloca"), Some("buffer"));
        assert_eq!(sink_category("vsscanf"), Some("source"));
        // path-writing buffer sinks
        assert_eq!(sink_category("getwd"), Some("buffer"));
        assert_eq!(sink_category("realpath"), Some("buffer"));
        assert_eq!(sink_category("__realpath_chk"), Some("buffer"));
        // wide / variadic format sinks
        assert_eq!(sink_category("vfprintf"), Some("format"));
        assert_eq!(sink_category("__vfprintf_chk"), Some("format")); // the motivating hole
        assert_eq!(sink_category("wprintf"), Some("format"));
        assert_eq!(sink_category("fwprintf"), Some("format"));
        assert_eq!(sink_category("vwprintf"), Some("format"));
        assert_eq!(sink_category("vfwprintf"), Some("format"));
        // no-NUL-terminator + line sources
        assert_eq!(sink_category("readlink"), Some("source"));
        assert_eq!(sink_category("readlinkat"), Some("source"));
        assert_eq!(sink_category("fgets"), Some("source"));
        assert_eq!(sink_category("recvmsg"), Some("source"));
    }

    #[test]
    fn catalog_hit_stays_authoritative_miss_falls_back_to_hint() {
        let mut catalog = HashMap::new();
        let mut memcpy_roles = ModelRoles::default();
        memcpy_roles.sink_classes.push("overflow_len".into());
        catalog.insert("memcpy".to_string(), memcpy_roles);

        // Catalog hit: authoritative roles used verbatim, NOT flagged heuristic,
        // even though the name also matches the local sink heuristic.
        let (roles, heuristic) = resolve_roles("memcpy", "memcpy", &catalog, true);
        assert!(!heuristic);
        assert_eq!(roles.sink_classes, vec!["overflow_len".to_string()]);

        // Catalog miss while catalog is present: heuristic supplements and the
        // row is flagged as a hint (the motivating __vfprintf_chk gap).
        let (roles, heuristic) = resolve_roles("__vfprintf_chk", "__vfprintf_chk", &catalog, true);
        assert!(heuristic);
        assert_eq!(roles.sink_classes, vec!["format".to_string()]);

        // A catalog miss the heuristic also can't classify: no roles, no hint.
        let (roles, heuristic) = resolve_roles("some_opaque_fn", "some_opaque_fn", &catalog, true);
        assert!(!heuristic);
        assert!(roles.sink_classes.is_empty() && !roles.source);
    }

    #[test]
    fn no_catalog_mode_does_not_flag_rows_as_hints() {
        // model_backed=false → full heuristic-fallback mode. Rows still classify
        // but are NOT individually marked hints (the footer discloses provenance
        // globally), so the whole list isn't dimmed.
        let empty = HashMap::new();
        let (roles, heuristic) = resolve_roles("strcpy", "strcpy", &empty, false);
        assert!(!heuristic);
        assert_eq!(roles.sink_classes, vec!["buffer".to_string()]);
    }

    #[test]
    fn new_sinks_do_not_overflag_lookalikes() {
        // bounded/benign neighbours of the new entries must stay clean
        assert_eq!(sink_category("getcwd"), None); // takes a size arg
        assert_eq!(sink_category("realpathname"), None); // substring, not exact
        // `wprintf`/`realpath`/`getwd` are exact-only (not segment tokens), so a
        // wrapper suffix does not flag them
        assert_eq!(sink_category("wprintf_wrapper"), None);
        assert_eq!(sink_category("realpath_cached"), None);
        // but `readlinkat` *is* a segment source token, so a wrapper does flag
        assert_eq!(sink_category("readlinkat_helper"), Some("source"));
    }

    #[test]
    fn catches_wrappers_at_segment_boundaries() {
        assert_eq!(sink_category("tsk_sys_System"), Some("command"));
        assert_eq!(sink_category("my_strcpy_safe"), Some("buffer"));
        assert_eq!(sink_category("bsp_MemCpy"), Some("buffer"));
    }

    #[test]
    fn catches_source_wrappers_but_not_benign_read_segments() {
        // specific tokens flag as sources on any whole segment
        assert_eq!(sink_category("sock_recvfrom"), Some("source"));
        assert_eq!(sink_category("svc_fgets"), Some("source"));
        assert_eq!(sink_category("app_readlink"), Some("source"));
        assert_eq!(sink_category("cfg_getenv"), Some("source"));
        // colliding tokens flag only as the *trailing* segment
        assert_eq!(sink_category("net_recv"), Some("source"));
        assert_eq!(sink_category("safe_fread"), Some("source"));
        assert_eq!(sink_category("uart_scanf"), Some("source"));
        // …and are rejected mid-name, where they're benign firmware identifiers
        // (real collisions surfaced by an adversarial review)
        assert_eq!(sink_category("rtw_init_recv_priv"), None); // recv init state
        assert_eq!(sink_category("scanf_float"), None); // newlib internal
        assert_eq!(sink_category("spi_mem_fread_qio"), None); // flash register
        // `read` is intentionally NOT a segment token — these stay clean
        assert_eq!(sink_category("reg_read"), None);
        assert_eq!(sink_category("spi_read"), None);
        assert_eq!(sink_category("read_config"), None);
        assert_eq!(sink_category("thread_create"), None);
        // `sscanf` is a propagator, not an origin — not inferred from a wrapper
        assert_eq!(sink_category("proto_sscanf"), None);
        // partial/substring tokens don't flag — must be a whole segment
        assert_eq!(sink_category("get_environment"), None);
        assert_eq!(sink_category("recovery"), None);
    }

    #[test]
    fn leaves_benign_imports_unflagged() {
        assert_eq!(sink_category("malloc"), None);
        assert_eq!(sink_category("targets"), None);
        assert_eq!(sink_category("SysDbClientOpen"), None);
        assert_eq!(sink_category("pthread_mutex_lock"), None);
        // the `system` false-positives the review caught
        assert_eq!(sink_category("__system_property_get"), None);
        assert_eq!(sink_category("filesystem"), None);
        assert_eq!(sink_category("get_system_info"), None);
    }
}
