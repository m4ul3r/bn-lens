//! Ratatui rendering for the code viewer and its modal popups.

use super::{CfgExpand, HotKind, Popup, View, Viewer};
use crate::ctx::Ctx;
use crate::theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::collections::HashMap;

const GUTTER_WIDTH: u16 = 7; // "NNNN │ "

impl Viewer {
    pub fn render(&mut self, area: Rect, buffer: &mut Buffer, ctx: &Ctx) {
        let height = area.height as usize;
        let width = area.width as usize;
        let stack_panel = self.stack_view.panel_rect(area);
        let code_width = stack_panel
            .map(|panel| panel.x.saturating_sub(area.x) as usize)
            .unwrap_or(width);
        // The 2D CFG graph has its own colored, box-navigable, pannable renderer.
        if self.cfg_graph_view.is_some() {
            self.render_cfg_graph(area, buffer, ctx);
            return;
        }
        let body_height = height.saturating_sub(3);
        self.cline = self.cline.min(self.lines.len().saturating_sub(1));
        if self.cline < self.top {
            self.top = self.cline;
        } else if self.cline >= self.top + body_height {
            self.top = self.cline + 1 - body_height;
        }

        let candidate = self.cur_span();
        // A popup is modal: recede the backdrop so no selection styling spills
        // around the box.
        let modal = !matches!(self.popup, Popup::None) || self.stack_view.is_modal(area);
        let active_local = self
            .stack_view
            .selected_name()
            .map(str::to_string)
            .or_else(|| {
                candidate.and_then(|index| {
                    let span = &self.spans[index];
                    (span.kind == HotKind::Local).then(|| span.target.clone())
                })
            });
        let (visual_low, visual_high) = if self.vmode {
            (self.vanchor.min(self.cline), self.vanchor.max(self.cline))
        } else {
            (usize::MAX, 0)
        };

        crate::ui::render_bar(buffer, area.x, area.y, width, &crate::ui::crumbs(ctx));

        // Row 1: search prompt, status, visual banner, or current location.
        let address = ctx
            .addr_by_name
            .get(&self.name)
            .cloned()
            .unwrap_or_default();
        let kind = self.view.label();
        let dim = Style::default().add_modifier(Modifier::DIM);
        if let Some(query) = &self.search_input {
            crate::ui::put_str(
                buffer,
                area.x,
                area.y + 1,
                format!(" /{query}"),
                code_width,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        } else if !self.status.is_empty() {
            crate::ui::put_str(
                buffer,
                area.x,
                area.y + 1,
                &self.status,
                code_width,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            );
        } else if self.vmode {
            crate::ui::put_str(
                buffer,
                area.x,
                area.y + 1,
                format!(
                    " ● VISUAL · {} lines selected",
                    visual_high - visual_low + 1
                ),
                code_width,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        } else {
            let mut location = Vec::new();
            if !address.is_empty() {
                location.push(Span::styled(
                    format!(" {address}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                location.push(Span::styled("  ", dim));
            }
            location.push(Span::styled(
                format!("{kind}  "),
                Style::default().fg(Color::Blue),
            ));
            location.push(Span::styled(
                self.name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            location.push(Span::styled(
                format!("   {}/{}", self.cline + 1, self.lines.len()),
                dim,
            ));
            crate::ui::put_spans(buffer, area.x, area.y + 1, code_width, &location);
        }

        let hint = if self.stack_view.is_open() {
            " stack · j/k slots · Enter jump · r rename · S/q close · ? help"
        } else if self.search_input.is_some() {
            " type to find · Enter jump · Esc cancel · ? help"
        } else if self.vmode {
            " j/k extend · a ask · Esc cancel · ? help"
        } else {
            " j/k move · Tab hotspot · g act · r/;/t edit · a ask · / find · b back · ? help · q list"
        };
        crate::ui::render_bar(
            buffer,
            area.x,
            area.y + area.height.saturating_sub(1),
            width,
            &[Span::styled(
                hint,
                Style::default().add_modifier(Modifier::DIM),
            )],
        );

        let search_lower = self.search.to_lowercase();
        self.screen_tgts.clear();
        let hotspot_at: HashMap<(usize, usize), usize> = self
            .spans
            .iter()
            .enumerate()
            .map(|(index, span)| ((span.line, span.col), index))
            .collect();

        let bottom = area.y + area.height.saturating_sub(1);
        let right = area.x + code_width as u16;
        let mut y = area.y + 2;
        let mut line = self.top;
        while y < bottom && line < self.lines.len() {
            let current = line == self.cline;
            let selected = visual_low <= line && line <= visual_high;
            let search_match = !search_lower.is_empty()
                && self.line_text(line).to_lowercase().contains(&search_lower);
            let marker = if current {
                "▸"
            } else if selected {
                "┃"
            } else if search_match {
                "◆"
            } else {
                " "
            };
            let gutter = format!("{:>4}{marker}│ ", line + 1);
            let gutter_style = if modal {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::DIM)
            } else if selected {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::REVERSED)
            } else if current {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if search_match {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::DIM)
            };
            crate::ui::put_str(
                buffer,
                area.x,
                y,
                gutter,
                GUTTER_WIDTH as usize,
                gutter_style,
            );

            let continuation_gutter = format!("    {}↳ ", if selected { "┃" } else { " " });
            let continuation_style = if selected {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::DIM)
            };

            let mut x = area.x + GUTTER_WIDTH;
            let mut col = 0usize;
            'segments: for segment in &self.lines[line] {
                let hotspot = hotspot_at.get(&(line, col)).copied();
                let style = if modal {
                    Style::default().add_modifier(Modifier::DIM)
                } else if let Some(index) = hotspot {
                    let span = &self.spans[index];
                    let sibling = span.kind == HotKind::Local
                        && active_local.as_deref() == Some(span.target.as_str());
                    if Some(index) == candidate {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else if sibling {
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    } else {
                        match span.kind {
                            HotKind::Func => Style::default()
                                .fg(theme::FUNC)
                                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                            HotKind::Data => Style::default()
                                .fg(theme::DATA)
                                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                            HotKind::Addr => Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                            HotKind::Str => Style::default()
                                .fg(Color::Magenta)
                                .add_modifier(Modifier::UNDERLINED),
                            HotKind::Local => Style::default().fg(Color::Gray),
                        }
                    }
                } else if matches!(self.view, View::Mlil | View::Disasm | View::Cfg) {
                    // MLIL/disasm and the CFG list (which shows disassembly) use
                    // the muted asm palette; decompile uses the pseudo-C one.
                    theme::asm_style(segment.kind)
                } else {
                    theme::tok_style(segment.kind)
                };

                let chars: Vec<char> = segment.text.chars().collect();
                let mut char_index = 0;
                let mut first_chunk = true;
                while char_index < chars.len() {
                    if x >= right {
                        y += 1;
                        if y >= bottom {
                            break 'segments;
                        }
                        crate::ui::put_str(
                            buffer,
                            area.x,
                            y,
                            &continuation_gutter,
                            GUTTER_WIDTH as usize,
                            continuation_style,
                        );
                        x = area.x + GUTTER_WIDTH;
                    }
                    let space = (right - x) as usize;
                    let chunk: String = chars[char_index..(char_index + space).min(chars.len())]
                        .iter()
                        .collect();
                    let chunk_len = chunk.chars().count();
                    crate::ui::put_str(buffer, x, y, &chunk, space, style);
                    if first_chunk {
                        if let Some(index) = hotspot {
                            self.screen_tgts.push((x, x + chunk_len as u16, y, index));
                        }
                    }
                    first_chunk = false;
                    char_index += chunk_len;
                    x += chunk_len as u16;
                    col += chunk_len;
                }
            }
            y += 1;
            line += 1;
        }

        self.render_popup(area, buffer, ctx);
        if matches!(self.popup, Popup::None) && self.stack_view.is_open() {
            self.stack_view.render(area, buffer, &self.name);
        }
    }

    /// Render the 2D CFG graph: the colored box-and-arrow canvas with the
    /// selected block highlighted. Navigated by block (hjkl), not by line.
    fn render_cfg_graph(&mut self, area: Rect, buffer: &mut Buffer, ctx: &Ctx) {
        let Some(mut g) = self.cfg_graph_view.take() else {
            return;
        };
        let width = area.width as usize;
        crate::ui::render_bar(buffer, area.x, area.y, width, &crate::ui::crumbs(ctx));

        // Row 1: location + colour legend.
        let dim = Style::default().add_modifier(Modifier::DIM);
        let sel_addr = g.data.blocks.get(g.sel).map(|b| b.addr).unwrap_or(0);
        let info = vec![
            Span::styled(
                format!(" {sel_addr:#x}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  cfg  ", Style::default().fg(Color::Blue)),
            Span::styled(
                self.name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   block {}/{}", g.sel + 1, g.data.block_count),
                dim,
            ),
            Span::styled("     ", dim),
            Span::styled("● true", Style::default().fg(Color::Green)),
            Span::styled("  ", dim),
            Span::styled("● false", Style::default().fg(Color::Red)),
            Span::styled("  ", dim),
            Span::styled("● branch", Style::default().fg(Color::Blue)),
        ];
        crate::ui::put_spans(buffer, area.x, area.y + 1, width, &info);

        let hint = " hjkl move · PgUp/Dn block · Enter read · Space list · i/v cycle · q close";
        crate::ui::render_bar(
            buffer,
            area.x,
            area.y + area.height.saturating_sub(1),
            width,
            &[Span::styled(hint, dim)],
        );

        let body_top = area.y + 2;
        let bottom = area.y + area.height.saturating_sub(1);
        let view_h = bottom.saturating_sub(body_top) as usize;

        // The graph is a canvas with empty padding above and below, so it can be
        // panned freely (a block can sit anywhere in the viewport, not just be
        // clamped tight to the content). `g.top` is an offset into this padded
        // virtual space; the real graph occupies rows [pad_v, pad_v + h).
        let pad_v = (view_h / 2).max(4);
        let virt_h = g.data.h + 2 * pad_v;
        // Scroll-off margin so the selection isn't jammed against an edge.
        let margin = 3usize;

        // First render of a freshly-built graph: rest the entry near the top (a
        // few rows of padding above), rather than floating in the middle of the
        // top pad. Panning up from here reveals the rest of the pad.
        if g.top == usize::MAX {
            g.top = pad_v.saturating_sub(margin);
        }

        // In follow-mode (after hjkl) the viewport tracks the selection; a mouse
        // pan clears follow so the canvas stays where the user dragged it.
        if g.follow {
            if let Some(sel) = g.data.blocks.get(g.sel) {
                let vtop = sel.top + pad_v;
                if vtop < g.top + margin {
                    g.top = vtop.saturating_sub(margin);
                }
                if vtop + sel.h + margin > g.top + view_h {
                    g.top = (vtop + sel.h + margin).saturating_sub(view_h);
                }
                if sel.left < g.left {
                    g.left = sel.left;
                }
                let need_r = sel.left + sel.w;
                if need_r > g.left + width {
                    g.left = need_r.saturating_sub(width);
                }
            }
        }
        // Clamp to the padded canvas so a pan can't lose the graph entirely.
        g.top = g.top.min(virt_h.saturating_sub(view_h.max(1)));
        g.left = g.left.min(g.data.w.saturating_sub(width.max(1)));

        // Record on-screen block rects for click hit-testing (in screen coords).
        self.cfg_hit.clear();
        for (i, b) in g.data.blocks.iter().enumerate() {
            let vtop = b.top + pad_v;
            let off_v = vtop + b.h <= g.top || vtop >= g.top + view_h;
            let off_h = b.left + b.w <= g.left || b.left >= g.left + width;
            if off_v || off_h {
                continue;
            }
            let sy = body_top as i64 + vtop as i64 - g.top as i64;
            let sx = area.x as i64 + b.left as i64 - g.left as i64;
            let x0 = sx.max(area.x as i64) as u16;
            let x1 = (sx + b.w as i64).min(area.x as i64 + width as i64) as u16;
            let y0 = sy.max(body_top as i64) as u16;
            let y1 = (sy + b.h as i64).min(bottom as i64) as u16;
            self.cfg_hit.push((x0, x1, y0, y1, i));
        }

        // Draw the visible canvas window, cell by cell (skipping the pad rows).
        let sel_rect = g.data.blocks.get(g.sel);
        for vy in 0..view_h {
            let crow = (g.top + vy) as i64 - pad_v as i64;
            if crow < 0 {
                continue; // top padding
            }
            let row = crow as usize;
            if row >= g.data.h {
                break; // bottom padding (and everything past it)
            }
            let y = body_top + vy as u16;
            for vx in 0..width {
                let col = g.left + vx;
                if col >= g.data.w {
                    break;
                }
                let (ch, color) = g.data.cell(row, col);
                if ch == ' ' {
                    continue;
                }
                let selected = sel_rect.is_some_and(|b| b.contains(row, col));
                crate::ui::put_str(
                    buffer,
                    area.x + vx as u16,
                    y,
                    ch.to_string(),
                    1,
                    cfg_cell_style(color, selected),
                );
            }
        }

        // Pan affordances: ‹ › on a mid-body row when the canvas extends off the
        // sides (the graph is wider than the pane and panned to follow selection).
        let mid = body_top + (view_h / 2) as u16;
        let mark = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        if g.left > 0 {
            crate::ui::put_str(buffer, area.x, mid, "‹", 1, mark);
        }
        if g.left + width < g.data.w {
            crate::ui::put_str(
                buffer,
                area.x + area.width.saturating_sub(1),
                mid,
                "›",
                1,
                mark,
            );
        }

        // Always-on top-left inspector for the highlighted block (syntax-
        // highlighted full instructions). Drawn last so it sits above the graph.
        render_cfg_expand(buffer, area, body_top, bottom, &mut g.expand);
        self.cfg_graph_view = Some(g);
    }

    fn render_popup(&self, area: Rect, buffer: &mut Buffer, ctx: &Ctx) {
        match &self.popup {
            Popup::None => {}
            Popup::Ask {
                label,
                preview,
                buf: input,
                ..
            } => {
                let box_width = (area.width.saturating_sub(6)).clamp(46, 110);
                let box_height = 8u16;
                let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
                let box_y = area.y + (area.height.saturating_sub(box_height)) / 2;
                crate::ui::draw_box(buffer, box_x, box_y, box_width, box_height, "Ask the agent");
                let destination = if ctx.agent_pane.is_empty() {
                    "→ (no launching pane — cannot send)".to_string()
                } else {
                    format!("→ sends to {}", ctx.agent_pane)
                };
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + 1,
                    destination,
                    (box_width - 4) as usize,
                    Style::default().fg(Color::Yellow),
                );
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + 2,
                    label,
                    (box_width - 4) as usize,
                    Style::default().fg(Color::Cyan),
                );
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + 3,
                    preview,
                    (box_width - 4) as usize,
                    Style::default().add_modifier(Modifier::DIM),
                );
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + 5,
                    format!("> {input}"),
                    (box_width - 4) as usize,
                    Style::default(),
                );
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + box_height - 1,
                    " Enter send · Esc cancel ",
                    (box_width - 4) as usize,
                    Style::default().add_modifier(Modifier::DIM),
                );
            }
            Popup::Peek {
                title,
                lines,
                off,
                focus,
            } => {
                let box_width = (area.width.saturating_sub(6)).clamp(50, 90);
                let box_height = (area.height.saturating_sub(4)).clamp(8, 22);
                let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
                let box_y = area.y + (area.height.saturating_sub(box_height)) / 2;
                crate::ui::draw_box(buffer, box_x, box_y, box_width, box_height, title);
                let view_height = (box_height - 3) as usize;
                let inner = (box_width - 4) as usize;
                for (row, line) in lines.iter().skip(*off).take(view_height).enumerate() {
                    let focused = *focus == Some(*off + row);
                    let style = if focused {
                        // A highlight bar across the full inner width for the
                        // focused statement (a decomp peek's use site).
                        Style::default()
                            .fg(Color::White)
                            .bg(crate::ui::HILITE_BG)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Yellow)
                    };
                    let y = box_y + 1 + row as u16;
                    if focused {
                        crate::ui::put_str(buffer, box_x + 2, y, " ".repeat(inner), inner, style);
                    }
                    crate::ui::put_str(buffer, box_x + 2, y, line, inner, style);
                }
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + box_height - 1,
                    " j/k scroll · ? help · q close ",
                    (box_width - 4) as usize,
                    Style::default().add_modifier(Modifier::DIM),
                );
            }
            Popup::Rename {
                old,
                buf: input,
                err,
                scope,
            } => {
                let box_width = (area.width.saturating_sub(6)).clamp(46, 90);
                let box_height = 7u16;
                let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
                let box_y = area.y + (area.height.saturating_sub(box_height)) / 2;
                let title = match scope {
                    super::RenameScope::Local => "rename local  (live in the bn instance)",
                    super::RenameScope::Symbol => "rename function  (live in the bn instance)",
                };
                crate::ui::draw_box(buffer, box_x, box_y, box_width, box_height, title);
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + 2,
                    format!("{old}  →"),
                    (box_width - 4) as usize,
                    Style::default().fg(Color::Cyan),
                );
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + 3,
                    format!("> {input}"),
                    (box_width - 4) as usize,
                    Style::default(),
                );
                if !err.is_empty() {
                    crate::ui::put_str(
                        buffer,
                        box_x + 2,
                        box_y + 4,
                        format!("✗ {err}"),
                        (box_width - 4) as usize,
                        Style::default().fg(Color::Red),
                    );
                }
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + box_height - 1,
                    " Enter rename · Esc cancel · ? help ",
                    (box_width - 4) as usize,
                    Style::default().add_modifier(Modifier::DIM),
                );
            }
            Popup::Comment { target, buf: input } => {
                self.render_input_box(
                    area,
                    buffer,
                    "comment  (live in the bn instance)",
                    &format!("comment {}", target.label()),
                    input,
                    " Enter set · Esc cancel · ? help ",
                );
            }
            Popup::Tag { target, buf: input } => {
                self.render_input_box(
                    area,
                    buffer,
                    "bookmark  (Bookmarks tag, live)",
                    &format!("bookmark {}  ·  optional note", target.label()),
                    input,
                    " Enter add · Esc cancel · ? help ",
                );
            }
        }
    }

    /// A one-line text-entry modal: title box, a cyan target line, the input,
    /// and a dim footer. Shared by the comment and tag popups.
    fn render_input_box(
        &self,
        area: Rect,
        buffer: &mut Buffer,
        title: &str,
        target_line: &str,
        input: &str,
        footer: &str,
    ) {
        let box_width = (area.width.saturating_sub(6)).clamp(46, 100);
        let box_height = 7u16;
        let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
        let box_y = area.y + (area.height.saturating_sub(box_height)) / 2;
        crate::ui::draw_box(buffer, box_x, box_y, box_width, box_height, title);
        crate::ui::put_str(
            buffer,
            box_x + 2,
            box_y + 2,
            target_line,
            (box_width - 4) as usize,
            Style::default().fg(Color::Cyan),
        );
        crate::ui::put_str(
            buffer,
            box_x + 2,
            box_y + 3,
            format!("> {input}"),
            (box_width - 4) as usize,
            Style::default(),
        );
        crate::ui::put_str(
            buffer,
            box_x + 2,
            box_y + box_height - 1,
            footer,
            (box_width - 4) as usize,
            Style::default().add_modifier(Modifier::DIM),
        );
    }
}

/// Render the always-on block inspector: the selected block's instructions,
/// syntax-highlighted with the disasm palette, in a content-sized box pinned to
/// the top-left. Caps height so the graph stays visible underneath/around it;
/// long blocks scroll with PgUp/PgDn (or the wheel when the pointer is over the
/// panel). Records the panel's screen bounds on `exp.hit` for mouse hit-testing.
fn render_cfg_expand(
    buffer: &mut Buffer,
    area: Rect,
    body_top: u16,
    bottom: u16,
    exp: &mut CfgExpand,
) {
    let avail_h = bottom.saturating_sub(body_top) as usize;
    // Leave at least half the body for the graph so the panel never eats the
    // whole canvas; still tall enough to show a useful window of insns.
    let max_h = avail_h.max(4).min((avail_h / 2).max(8).min(20));
    let inner_w = exp
        .lines
        .iter()
        .map(|l| l.iter().map(|s| s.text.chars().count()).sum::<usize>())
        .max()
        .unwrap_or(24)
        .clamp(24, (area.width as usize).saturating_sub(6).min(96));
    let panel_w = (inner_w + 4) as u16;
    let panel_h = (exp.lines.len() + 2).clamp(4, max_h) as u16;
    let panel_x = area.x;
    let panel_y = body_top;
    exp.hit = Some((panel_x, panel_y, panel_w, panel_h));
    // Solid black fill so the highlighted block's instructions read like a
    // terminal dump against the graph canvas.
    let bg = Color::Black;
    let fg = crate::ui::POPUP_FG;
    crate::ui::draw_box_colored(
        buffer, panel_x, panel_y, panel_w, panel_h, &exp.title, bg, fg,
    );

    let view_rows = (panel_h as usize).saturating_sub(2);
    // Clamp scroll if the panel shrank (e.g. resize).
    let max_off = exp.lines.len().saturating_sub(view_rows.max(1));
    if exp.off > max_off {
        exp.off = max_off;
    }
    for (i, line) in exp.lines.iter().skip(exp.off).take(view_rows).enumerate() {
        let spans: Vec<Span> = line
            .iter()
            .map(|seg| {
                let mut st = theme::asm_style(seg.kind);
                if st.fg.is_none() {
                    st = st.fg(fg);
                }
                Span::styled(seg.text.clone(), st.bg(bg))
            })
            .collect();
        crate::ui::put_spans(buffer, panel_x + 2, panel_y + 1 + i as u16, inner_w, &spans);
    }

    // Scroll affordance when content overflows the panel.
    let more = exp.off + view_rows < exp.lines.len();
    if more || exp.off > 0 {
        let hint = " PgUp/Dn scroll ";
        let hlen = hint.chars().count() as u16;
        if hlen + 2 < panel_w {
            crate::ui::put_str(
                buffer,
                panel_x + panel_w - hlen - 1,
                panel_y + panel_h - 1,
                hint,
                hlen as usize,
                Style::default()
                    .fg(Color::Cyan)
                    .bg(bg)
                    .add_modifier(Modifier::DIM),
            );
        }
    }
}

/// Style for a CFG canvas cell from its colour class. Edges are bold-coloured
/// (green true / red false / blue other); the selected block's border turns
/// yellow and its text goes bold so the box pops.
fn cfg_cell_style(col: u8, selected: bool) -> Style {
    use crate::cfg;
    let base = match col {
        cfg::C_TRUE => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        cfg::C_FALSE => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        cfg::C_OTHER => Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
        cfg::C_ADDR => Style::default().fg(crate::theme::ADDR),
        cfg::C_BORDER => Style::default().fg(Color::Gray),
        _ => Style::default(),
    };
    if selected {
        match col {
            cfg::C_BORDER => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            _ => base.add_modifier(Modifier::BOLD),
        }
    } else if col == cfg::C_BORDER {
        base.add_modifier(Modifier::DIM)
    } else {
        base
    }
}
