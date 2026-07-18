//! The strings view: an address-ordered, filterable list of every string bn
//! recovered. `Enter`/`x` cross-references the selected string (its uses land in
//! the viewer), so it doubles as a "where is this text used?" entry point.

use crate::ctx::Ctx;
use crate::picker::Action;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

struct StrItem {
    addr: String,
    content: String,
    /// Classified once at build time — render/filter run every frame.
    fmt: FmtKind,
}

enum Mode {
    Normal,
    Search,
}

/// A scrollable overlay showing where a string is used (raw `bn xrefs`).
struct Usage {
    title: String,
    addr: String,
    lines: Vec<String>,
    off: usize,
}

pub struct StringsList {
    items: Vec<StrItem>,
    awidth: usize,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sel: usize,
    top: usize,
    pending_g: bool,
    /// `f` filter: show only printf format strings (the printf-sink surface).
    fmt_only: bool,
    usage: Option<Usage>,
}

fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok()
}

/// A string's printf-format character, for VR triage. Format strings are what
/// flow into the printf-family sinks, so they're the interesting attack surface;
/// `%n` is the write primitive that turns a format-string bug into an arbitrary
/// write, so it's called out separately.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FmtKind {
    None,
    /// Has at least one real conversion (`%s`, `%d`, …).
    Conv,
    /// Contains `%n` — a format-string *write* primitive.
    WriteN,
}

/// Classify `s` as a printf format string. Scans for `%` conversions, tolerating
/// `+`/`#` flags, width/precision, length modifiers, and positional `$`, and
/// skipping `%%`.
///
/// Two deliberate precision choices keep natural text from reading as a format
/// string: the space flag is NOT honored (so `"50% off"` / `"100% done"` don't
/// match), and a conversion char only counts when it's *terminal* — end of
/// string or followed by a non-letter. That rejects word-shaped text like
/// `"%name%"` (would otherwise be a `%n` write-primitive false positive),
/// `"%usage"`, and URL-encoded `"%2Fpath"`, while still matching the common real
/// forms `"%s"`, `"%02x%02x"`, `"%d.%d.%d"`, and `"%s: %d"`. It identifies
/// printf-shaped text, not proven printf-sink provenance — a triage lens.
fn format_kind(s: &str) -> FmtKind {
    let b = s.as_bytes();
    let mut kind = FmtKind::None;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'%' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if j < b.len() && b[j] == b'%' {
            i = j + 1; // literal %%
            continue;
        }
        // flags (+/#, not the ambiguous space), width/precision, positional `$`,
        // and length modifiers.
        while j < b.len()
            && matches!(b[j], b'+' | b'#' | b'-' | b'.' | b'*' | b'$' | b'0'..=b'9'
                | b'l' | b'h' | b'L' | b'z' | b'j' | b't' | b'q')
        {
            j += 1;
        }
        // A conversion char only counts when terminal (end, or a non-letter
        // next) — otherwise it's a word like `%name` / `%usage`, not a spec.
        let terminal = j + 1 >= b.len() || !b[j + 1].is_ascii_alphabetic();
        if j < b.len() && terminal {
            match b[j] {
                b'n' => return FmtKind::WriteN, // strongest signal — stop here
                b'd' | b'i' | b'o' | b'u' | b'x' | b'X' | b'e' | b'E' | b'f' | b'F' | b'g'
                | b'G' | b'a' | b'A' | b'c' | b's' | b'p' => kind = FmtKind::Conv,
                _ => {}
            }
        }
        i = j + 1;
    }
    kind
}

impl StringsList {
    pub fn new(ctx: &Ctx) -> Self {
        let items = Self::build(ctx);
        let awidth = items
            .iter()
            .map(|it| it.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);
        StringsList {
            items,
            awidth,
            filter: String::new(),
            prev_filter: String::new(),
            mode: Mode::Normal,
            sel: 0,
            top: 0,
            pending_g: false,
            fmt_only: false,
            usage: None,
        }
    }

    /// Re-pull strings from a rebuilt ctx (keeps the filter, snaps the cursor).
    pub fn refresh(&mut self, ctx: &Ctx) {
        self.items = Self::build(ctx);
        self.awidth = self
            .items
            .iter()
            .map(|it| it.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);
        // Keep the cursor valid if the rebuilt list shrank.
        self.sel = self.sel.min(self.filtered().len().saturating_sub(1));
        self.top = self.top.min(self.sel);
    }

    /// Deduped by address. bn emits the same content at several addresses
    /// (`.rodata`/`.dynstr`); we keep each distinct address. Ordered for triage:
    /// real `.rodata` literals first, symbol-table/header noise last (by section
    /// rank), then by address within each rank — so the useful strings lead
    /// instead of the ELF header junk a pure address sort surfaces first.
    fn build(ctx: &Ctx) -> Vec<StrItem> {
        let mut seen = std::collections::HashSet::new();
        let mut items: Vec<StrItem> = ctx
            .bn
            .strings()
            .into_iter()
            .filter(|(_, addr)| addr.starts_with("0x") && seen.insert(addr.clone()))
            .map(|(content, addr)| {
                let fmt = format_kind(&content);
                StrItem { addr, content, fmt }
            })
            .collect();
        items.sort_by_key(|it| {
            let addr = parse_hex(&it.addr).unwrap_or(0);
            (ctx.string_rank(addr), addr)
        });
        items
    }

    fn filtered(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                let text_ok = f.is_empty()
                    || it.content.to_lowercase().contains(&f)
                    || it.addr.contains(&f);
                let fmt_ok = !self.fmt_only || it.fmt != FmtKind::None;
                text_ok && fmt_ok
            })
            .collect()
    }

    fn fmt_count(&self) -> usize {
        self.items
            .iter()
            .filter(|it| it.fmt != FmtKind::None)
            .count()
    }

    fn move_sel(&mut self, delta: i64) {
        let len = self.filtered().len() as i64;
        if len == 0 {
            return;
        }
        self.sel = (self.sel as i64 + delta).clamp(0, len - 1) as usize;
    }

    /// True while the search filter is capturing raw text (App must not steal
    /// `m`/`?`/`^R`).
    pub fn is_searching(&self) -> bool {
        matches!(self.mode, Mode::Search)
    }

    /// True while the usage popup owns input.
    pub fn popup_open(&self) -> bool {
        self.usage.is_some()
    }

    fn current(&self) -> Option<&StrItem> {
        let rows = self.filtered();
        rows.get(self.sel).map(|&i| &self.items[i])
    }

    fn current_addr(&self) -> Option<String> {
        self.current().map(|it| it.addr.clone())
    }

    /// `p`: peek *where the string is used in code or data*. Parse `bn xrefs`,
    /// decompile each referencing function once (`--addresses`), and show exact
    /// disassembly plus approximate pseudo-C at each callsite, grouped by
    /// function, plus any data refs. (`x`/Enter opens the full xrefs listing.)
    fn open_usage(&mut self, ctx: &Ctx) {
        let Some(item) = self.current() else { return };
        let (addr, content) = (item.addr.clone(), item.content.clone());
        self.usage = Some(Usage {
            title: format!("used in code · \"{}\"", ellipsize(&content, 34)),
            lines: crate::usage::report(ctx, &addr, &content),
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
            // q is the only quit. Esc backs out: clear the filter if set, else
            // return to the Symbols list (never closes the pane).
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc => {
                // Back out one step: drop the text filter, then the fmt filter,
                // then return to Symbols (never closes the pane).
                if !self.filter.is_empty() {
                    self.filter.clear();
                } else if self.fmt_only {
                    self.fmt_only = false;
                } else {
                    return Action::Home;
                }
                self.sel = 0;
                self.top = 0;
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
            KeyCode::Char('/') => {
                self.prev_filter = self.filter.clone();
                self.filter.clear();
                self.mode = Mode::Search;
                self.sel = 0;
            }
            KeyCode::Char('f') => {
                self.fmt_only = !self.fmt_only;
                self.sel = 0;
                self.top = 0;
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
        bar.push(Span::styled(
            format!(
                "   strings  {}/{}  · {} format",
                rows.len(),
                self.items.len(),
                self.fmt_count()
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
                match (self.fmt_only, self.filter.is_empty()) {
                    (true, true) => " format strings only".to_string(),
                    (true, false) => format!(" format only · filter: {}", self.filter),
                    (false, true) => String::new(),
                    (false, false) => format!(" filter: {}", self.filter),
                },
                " j/k move · / search · f format · p usage · Enter/x xrefs · m menu · v next list · i switch · q quit",
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
            // `%n` is a format-string write primitive — a genuine red flag.
            let write_n = it.fmt == FmtKind::WriteN;
            let tag = if write_n { "  ⚠%n" } else { "" };
            if is_sel {
                let text =
                    format!("{:<aw$}  \"{}\"{tag}", it.addr, it.content, aw = self.awidth);
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
            let mut spans = vec![
                Span::styled(
                    format!("{:<aw$}", it.addr, aw = self.awidth),
                    Style::default()
                        .fg(crate::theme::ADDR)
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  \"{}\"", it.content),
                    Style::default().fg(Color::Magenta),
                ),
            ];
            if write_n {
                spans.push(Span::styled(
                    tag.to_string(),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
            crate::ui::put_spans(buf, x0, y, w, &spans);
        }

        self.render_usage(area, buf);
    }

    /// The `p` overlay: raw `bn xrefs` for the selected string — its callsites.
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
    use super::{format_kind, FmtKind};

    #[test]
    fn detects_real_conversions() {
        assert_eq!(format_kind("value=%d"), FmtKind::Conv);
        assert_eq!(format_kind("%s: %d bytes"), FmtKind::Conv);
        assert_eq!(format_kind("ptr %p width %-10s prec %.2f"), FmtKind::Conv);
        assert_eq!(format_kind("%ld / %llu / %zu"), FmtKind::Conv);
    }

    #[test]
    fn flags_write_primitive() {
        assert_eq!(format_kind("overflow %n here"), FmtKind::WriteN);
        // %n wins even alongside ordinary conversions
        assert_eq!(format_kind("%s then %n"), FmtKind::WriteN);
        assert_eq!(format_kind("%-8n"), FmtKind::WriteN);
    }

    #[test]
    fn plain_text_and_percent_signs_are_not_formats() {
        assert_eq!(format_kind("just a string"), FmtKind::None);
        assert_eq!(format_kind("50% off"), FmtKind::None); // space flag not honored
        assert_eq!(format_kind("100% done"), FmtKind::None);
        assert_eq!(format_kind("progress: 42%"), FmtKind::None); // trailing %
        assert_eq!(format_kind("literal %% only"), FmtKind::None); // escaped
        assert_eq!(format_kind(""), FmtKind::None);
    }

    #[test]
    fn terminal_rule_rejects_word_shaped_text() {
        // conversion char must be terminal (end / non-letter next), so these
        // word/template/URL shapes are NOT formats (adversarial-review cases)
        assert_eq!(format_kind("%name%"), FmtKind::None); // was a %n false positive
        assert_eq!(format_kind("%node_id"), FmtKind::None);
        assert_eq!(format_kind("%usage"), FmtKind::None);
        assert_eq!(format_kind("95%ile"), FmtKind::None);
        assert_eq!(format_kind("%2Fpath"), FmtKind::None); // URL-encoded
        // real forms with a separator after the conversion still match
        assert_eq!(format_kind("%02x%02x"), FmtKind::Conv);
        assert_eq!(format_kind("%s/%s"), FmtKind::Conv);
        assert_eq!(format_kind("%s\n"), FmtKind::Conv);
    }

    #[test]
    fn positional_and_flag_forms() {
        // positional `$` write primitive
        assert_eq!(format_kind("%1$n"), FmtKind::WriteN);
        assert_eq!(format_kind("%2$hhn"), FmtKind::WriteN);
        // `+`/`#` flags are honored (space is not)
        assert_eq!(format_kind("%#08x"), FmtKind::Conv);
        assert_eq!(format_kind("%+d"), FmtKind::Conv);
        // documented false-negative trade-off: a conversion glued directly to
        // trailing literal letters (no separator) is not detected
        assert_eq!(format_kind("%dms"), FmtKind::None);
    }
}
