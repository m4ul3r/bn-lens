//! Shared drawing helpers: coloured span runs and the header bar.

use crate::ctx::Ctx;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Background of the header bar. Solid black for maximum text contrast
/// (matches the CFG expand panel).
pub const BAR_BG: Color = Color::Black;

/// Popup panel colours — a solid black fill, with a light default foreground
/// so content that sets no colour of its own stays legible (and doesn't fall
/// back to a terminal default fg that could vanish on the bg).
pub const POPUP_BG: Color = Color::Black;
pub const POPUP_FG: Color = Color::Rgb(216, 221, 233);

/// Selection/focus bar background inside a popup (an accent blue that reads on
/// the panel) — e.g. the focused statement of a decomp peek.
pub const HILITE_BG: Color = Color::Rgb(58, 78, 130);

/// Shorten a bndb cache selector to a human name for the header. bn's cache
/// stores targets as `<name>.<16-hex-hash>.bndb` (e.g.
/// `sample_svc.deadbeefcafebabe.bndb`); the hash is noise in the crumbs. Strip
/// the `.bndb` suffix and a trailing hex-hash segment, leaving `sample_svc`.
/// bn still resolves the short name as a `-t` selector, so it stays copyable.
/// Anything that isn't a cache `.bndb` selector is returned unchanged. Shared
/// with the switcher (column display + filter matching on the short form).
pub fn clean_target_label(sel: &str) -> String {
    let Some(base) = sel.strip_suffix(".bndb") else {
        return sel.to_string();
    };
    if let Some(dot) = base.rfind('.') {
        let (name, hash) = (&base[..dot], &base[dot + 1..]);
        if hash.len() >= 8 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return name.to_string();
        }
    }
    base.to_string()
}

/// The shared header "breadcrumbs": tool · `-i instance` · `-t target` · arch.
/// The `-i` is shown verbatim so an agent reading the pane can copy it into
/// `bn -i <>`; the `-t` target is cleaned of the bndb cache hash for legibility
/// (still a valid selector — see [`clean_target_label`]).
pub fn crumbs(ctx: &Ctx) -> Vec<Span<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut v = vec![
        Span::styled(" bn lens ", bold.fg(Color::Cyan)),
        Span::styled(" · ", dim),
        Span::styled("-i ", dim),
        Span::styled(ctx.instance_label.clone(), bold.fg(Color::Blue)),
    ];
    if !ctx.target.is_empty() {
        v.push(Span::styled("  -t ", dim));
        v.push(Span::styled(
            clean_target_label(&ctx.target),
            Style::default().fg(Color::White),
        ));
    }
    if !ctx.arch.is_empty() {
        v.push(Span::styled(
            format!("  · {}", ctx.arch),
            Style::default().fg(Color::Cyan),
        ));
    }
    if !ctx.analysis_complete() {
        v.push(Span::styled(
            format!(
                "  · ⚠ {} ANALYSIS",
                ctx.analysis_state.label().to_uppercase()
            ),
            bold.fg(Color::Yellow),
        ));
    }
    v
}

/// `set_stringn` that also clips *vertically*. ratatui clips x but panics on a
/// `y` outside the buffer, so every popup write (drawn at a box-relative `y`
/// that can fall below a short pane) must go through this to honour the
/// "clip, never panic" invariant.
pub fn put_str(buf: &mut Buffer, x: u16, y: u16, s: impl AsRef<str>, w: usize, style: Style) {
    let area = buf.area();
    if y >= area.top() && y < area.bottom() && x >= area.left() && x < area.right() {
        buf.set_stringn(x, y, s, w, style);
    }
}

/// Write coloured spans left-to-right, clipped to `max_w`.
pub fn put_spans(buf: &mut Buffer, x0: u16, y: u16, max_w: usize, spans: &[Span]) {
    let area = buf.area();
    if y < area.top() || y >= area.bottom() {
        return;
    }
    let mut x = x0;
    let end = x0 + max_w as u16;
    for s in spans {
        if x >= end {
            break;
        }
        let (nx, _) = buf.set_stringn(x, y, &s.content, (end - x) as usize, s.style);
        x = nx;
    }
}

/// A cleared, bordered box with a title (used by popups and the switcher).
pub fn draw_box(buf: &mut Buffer, x: u16, y: u16, w: u16, h: u16, title: &str) {
    draw_box_colored(buf, x, y, w, h, title, POPUP_BG, POPUP_FG);
}

/// Like [`draw_box`], but with an explicit fill colour (e.g. the CFG block
/// inspector uses solid black so the instructions read like a terminal dump).
pub fn draw_box_colored(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    title: &str,
    bg: Color,
    fg: Color,
) {
    if w < 2 || h < 2 {
        return;
    }
    let wu = w as usize;
    // Fill from a *reset* base (ratatui patches cell styles, so an all-unset
    // default would leave the highlighted cells underneath bleeding through),
    // then set the panel bg + a light default fg. Because writes only patch, any
    // content drawn on top keeps this bg unless it sets its own — so the whole
    // popup reads as one opaque, raised panel.
    let panel = Style::reset().bg(bg).fg(fg);
    // Borders start from a plain style (no reset), so `add_modifier(BOLD)` on the
    // title isn't cancelled by reset()'s "clear all modifiers".
    let cyan = Style::default().fg(Color::Cyan).bg(bg);
    for row in 0..h {
        put_str(buf, x, y + row, " ".repeat(wu), wu, panel);
    }
    // top: ┌─ title ─…─┐  (exactly w columns; box chars are width 1). Ellipsize a
    // title too wide to leave room for the closing corner, so the border always
    // ends in ┐ instead of a clipped, broken run.
    let budget = wu.saturating_sub(6); // "┌─ " + " " + "─┐"
    let title = if title.chars().count() > budget {
        format!(
            "{}…",
            title
                .chars()
                .take(budget.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        title.to_string()
    };
    let head = format!("┌─ {title} ");
    let dashes = wu.saturating_sub(head.chars().count() + 1); // +1 for the ┐
    let top = format!("{head}{}┐", "─".repeat(dashes));
    put_str(buf, x, y, top, wu, cyan.add_modifier(Modifier::BOLD));
    // bottom: └─…─┘
    let bottom = format!("└{}┘", "─".repeat(wu.saturating_sub(2)));
    put_str(buf, x, y + h - 1, bottom, wu, cyan);
    // sides
    for row in 1..h.saturating_sub(1) {
        put_str(buf, x, y + row, "│", 1, cyan);
        put_str(buf, x + w - 1, y + row, "│", 1, cyan);
    }
}

/// Fill row `y` with the bar background, then write `spans` on top of it (their
/// own fg colours, the bar's bg). Makes a solid title-bar look.
pub fn render_bar(buf: &mut Buffer, x0: u16, y: u16, width: usize, spans: &[Span]) {
    buf.set_stringn(x0, y, " ".repeat(width), width, Style::default().bg(BAR_BG));
    let mut x = x0;
    let end = x0 + width as u16;
    for s in spans {
        if x >= end {
            break;
        }
        let style = s.style.bg(BAR_BG);
        let (nx, _) = buf.set_stringn(x, y, &s.content, (end - x) as usize, style);
        x = nx;
    }
}

#[cfg(test)]
mod tests {
    use super::clean_target_label;

    #[test]
    fn strips_cache_hash_from_bndb_selector() {
        // bn's cache stores targets as <name>.<16-hex>.bndb
        assert_eq!(
            clean_target_label("sample_svc.deadbeefcafebabe.bndb"),
            "sample_svc"
        );
        assert_eq!(
            clean_target_label("netcfgd.0123456789abcdef.bndb"),
            "netcfgd"
        );
    }

    #[test]
    fn keeps_names_without_a_hash_segment() {
        // a plain .bndb (no hash) just loses the extension
        assert_eq!(clean_target_label("libfoo.bndb"), "libfoo");
        // dotted names whose last segment isn't a long hex hash are preserved
        assert_eq!(clean_target_label("lib.so.1.bndb"), "lib.so.1");
    }

    #[test]
    fn leaves_non_bndb_selectors_untouched() {
        assert_eq!(clean_target_label("my_binary"), "my_binary");
        assert_eq!(clean_target_label(""), "");
    }
}
