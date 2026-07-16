//! Shared drawing helpers: coloured span runs and the header bar.

use crate::ctx::Ctx;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Background of the header bar (a dark slate that reads on light + dark themes).
pub const BAR_BG: Color = Color::Rgb(38, 44, 66);

/// Popup panel colours — a slightly raised slate over the header, with a light
/// default foreground so content that sets no colour of its own stays legible
/// (and doesn't fall back to a terminal default fg that could vanish on the bg).
pub const POPUP_BG: Color = Color::Rgb(48, 55, 82);
pub const POPUP_FG: Color = Color::Rgb(216, 221, 233);

/// The shared header "breadcrumbs": tool · `-i instance` · `-t target` · arch.
/// The `-i`/`-t` are shown verbatim so an agent reading the pane can copy them
/// into `bn -i <> -t <>`; the target selector also carries the binary name.
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
        v.push(Span::styled(ctx.target.clone(), Style::default().fg(Color::White)));
    }
    if !ctx.arch.is_empty() {
        v.push(Span::styled(format!("  · {}", ctx.arch), Style::default().fg(Color::Cyan)));
    }
    v
}

/// Write coloured spans left-to-right, clipped to `max_w`.
pub fn put_spans(buf: &mut Buffer, x0: u16, y: u16, max_w: usize, spans: &[Span]) {
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
    if w < 2 || h < 2 {
        return;
    }
    let wu = w as usize;
    // Fill from a *reset* base (ratatui patches cell styles, so an all-unset
    // default would leave the highlighted cells underneath bleeding through),
    // then set the panel bg + a light default fg. Because writes only patch, any
    // content drawn on top keeps this bg unless it sets its own — so the whole
    // popup reads as one opaque, raised panel.
    let panel = Style::reset().bg(POPUP_BG).fg(POPUP_FG);
    // Borders start from a plain style (no reset), so `add_modifier(BOLD)` on the
    // title isn't cancelled by reset()'s "clear all modifiers".
    let cyan = Style::default().fg(Color::Cyan).bg(POPUP_BG);
    for row in 0..h {
        buf.set_stringn(x, y + row, " ".repeat(wu), wu, panel);
    }
    // top: ┌─ title ─…─┐  (exactly w columns; box chars are width 1)
    let head = format!("┌─ {title} ");
    let dashes = wu.saturating_sub(head.chars().count() + 1); // +1 for the ┐
    let top = format!("{head}{}┐", "─".repeat(dashes));
    buf.set_stringn(x, y, top, wu, cyan.add_modifier(Modifier::BOLD));
    // bottom: └─…─┘
    let bottom = format!("└{}┘", "─".repeat(wu.saturating_sub(2)));
    buf.set_stringn(x, y + h - 1, bottom, wu, cyan);
    // sides
    for row in 1..h.saturating_sub(1) {
        buf.set_stringn(x, y + row, "│", 1, cyan);
        buf.set_stringn(x + w - 1, y + row, "│", 1, cyan);
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
