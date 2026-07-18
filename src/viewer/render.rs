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

/// Inline display cap for a string-literal segment. Longer literals ellipsize
/// (the full content stays reachable via peek/xref) so a boilerplate string
/// can't push real code off screen across several wrapped rows.
const STR_DISPLAY_LIMIT: usize = 48;

/// Turn a tokenized pseudo-C line into coloured spans, dropping the first
/// `hoff` characters (horizontal pan). Splits inside a segment when the pan
/// lands mid-token so colours stay aligned to the panned text.
fn pan_syntax_spans(segs: &[crate::syntax::Seg], hoff: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for seg in segs {
        let len = seg.text.chars().count();
        if skipped + len <= hoff {
            skipped += len;
            continue;
        }
        let text: String = if skipped < hoff {
            let drop = hoff - skipped;
            skipped = hoff;
            seg.text.chars().skip(drop).collect()
        } else {
            seg.text.clone()
        };
        out.push(Span::styled(text, theme::tok_style(seg.kind)));
    }
    out
}

/// Hard-wrap a composer value and retain the visible tail. The ask field is a
/// single logical line, but hiding everything after the popup width made long
/// prompts impossible to review before sending.
fn tail_wrapped_lines(input: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = input.chars().collect();
    let mut lines = if chars.is_empty() {
        vec![String::new()]
    } else {
        chars
            .chunks(width)
            .map(|chunk| chunk.iter().collect())
            .collect::<Vec<String>>()
    };
    if lines.len() > max_lines {
        lines.drain(0..lines.len() - max_lines);
    }
    lines
}

/// Wrap `input` into `width`-char rows and return the visible window of up to
/// `max_lines` rows that keeps the caret (char index `cursor`) in view (the
/// window bottom-pins to the caret's row), the caret's position within the
/// window `(row, col)`, and whether the window includes the text's first row
/// (so only that row gets the `>` prompt marker).
fn wrap_cursor(
    input: &str,
    width: usize,
    max_lines: usize,
    cursor: usize,
) -> (Vec<String>, usize, usize, bool) {
    let width = width.max(1);
    let chars: Vec<char> = input.chars().collect();
    let mut rows: Vec<String> = if chars.is_empty() {
        vec![String::new()]
    } else {
        chars
            .chunks(width)
            .map(|chunk| chunk.iter().collect())
            .collect()
    };
    let cursor = cursor.min(chars.len());
    let (caret_row, caret_col) = (cursor / width, cursor % width);
    // A caret one past a full final row sits at the start of a fresh row.
    if caret_row >= rows.len() {
        rows.push(String::new());
    }
    let total = rows.len();
    let start = if caret_row + 1 > max_lines {
        caret_row + 1 - max_lines
    } else {
        0
    };
    let end = (start + max_lines).min(total);
    (
        rows[start..end].to_vec(),
        caret_row - start,
        caret_col,
        start == 0,
    )
}

impl Viewer {
    /// Screen rows a logical line occupies once wrapped to `avail` text columns
    /// (min 1). Mirrors the wrapping in `render`: a string literal is ellipsized
    /// to `STR_DISPLAY_LIMIT`, everything else counts its full character width.
    fn wrapped_rows(&self, line: usize, avail: usize) -> usize {
        if avail == 0 {
            return 1;
        }
        let width: usize = self
            .lines
            .get(line)
            .map(|segments| {
                segments
                    .iter()
                    .map(|segment| {
                        if segment.kind == crate::syntax::Tok::Str {
                            super::hotspots::truncate_str_segment(&segment.text, STR_DISPLAY_LIMIT)
                                .map(|shown| shown.chars().count())
                                .unwrap_or_else(|| segment.text.chars().count())
                        } else {
                            segment.text.chars().count()
                        }
                    })
                    .sum()
            })
            .unwrap_or(0);
        ((width + avail - 1) / avail).max(1)
    }

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
        let body_height = height.saturating_sub(3).max(1);
        self.cline = self.cline.min(self.lines.len().saturating_sub(1));
        // Keep the cursor on screen measured in *rendered rows*, not logical
        // lines: long MLIL/disasm lines wrap onto continuation rows and blank
        // block separators add lines, so the old line-count window let the
        // cursor scroll off the bottom. Walk up from the cursor, summing wrapped
        // row heights, to find the highest `top` whose rows still fit the body.
        let avail = code_width.saturating_sub(GUTTER_WIDTH as usize);
        let mut rows = self.wrapped_rows(self.cline, avail);
        let mut min_top = self.cline;
        while min_top > 0 {
            let above = self.wrapped_rows(min_top - 1, avail);
            if rows + above > body_height {
                break;
            }
            rows += above;
            min_top -= 1;
        }
        self.top = self.top.clamp(min_top, self.cline);

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
        // The CFG renders at the shared IL — surface it (`cfg·mlil`), since the
        // block text alone doesn't always give the level away.
        let kind = if self.view == View::Cfg {
            format!("cfg·{}", self.code_view.label())
        } else {
            self.view.label().to_string()
        };
        let dim = Style::default().add_modifier(Modifier::DIM);
        if let Some(query) = &self.goto_input {
            // Live completion: show the unique name Enter would jump to, or a
            // match count, replacing the static help once the user starts typing.
            let hint = self.goto_hint(ctx, query);
            let tail = if hint.is_empty() {
                "(goto 0x… / name · Enter · Esc)".to_string()
            } else {
                hint
            };
            crate::ui::put_str(
                buffer,
                area.x,
                area.y + 1,
                format!(" :{query}▏   {tail}"),
                code_width,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        } else if let Some(query) = &self.search_input {
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
        } else if let Some(hint) = self.hotspot_hint(ctx) {
            // A deliberately-selected hotspot: preview what it is and what `g`
            // does, so the action is legible without the colour highlight.
            crate::ui::put_str(
                buffer,
                area.x,
                area.y + 1,
                hint,
                code_width,
                Style::default()
                    .fg(Color::Cyan)
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
                ctx.display_name(&self.name).to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            location.push(Span::styled(
                format!("   {}/{}", self.cline + 1, self.lines.len()),
                dim,
            ));
            crate::ui::put_spans(buffer, area.x, area.y + 1, code_width, &location);
        }

        let hint = if self.stack_view.is_open() {
            " stack · j/k slots · Enter jump · n rename · S/q close · ? help"
        } else if self.search_input.is_some() {
            " type to find · Enter jump · Esc cancel · ? help"
        } else if self.vmode {
            " j/k extend · a ask · Esc cancel · ? help"
        } else {
            " j/k · w/b hotspot · W/B calls · g act · n/;/t · a ask · / find · : goto · i il · v cfg · ^O/^F hist · q · ? help"
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
            // A blank line (e.g. a basic-block separator in the linear views)
            // gets no row number, so the gap reads as a clean break.
            let gutter = if self.line_text(line).trim().is_empty() {
                format!("{:>4}{marker}│ ", "")
            } else {
                format!("{:>4}{marker}│ ", line + 1)
            };
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

                // Ellipsize an over-long string literal for display; the hotspot
                // keeps the full content. `full_len` lets us re-sync the logical
                // column afterwards so later hotspots on this line stay aligned.
                let full_len = segment.text.chars().count();
                let draw_text = if segment.kind == crate::syntax::Tok::Str {
                    super::hotspots::truncate_str_segment(&segment.text, STR_DISPLAY_LIMIT)
                } else {
                    None
                };
                let chars: Vec<char> = draw_text
                    .as_deref()
                    .unwrap_or(&segment.text)
                    .chars()
                    .collect();
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
                // Advance the logical column past any elided characters so
                // `hotspot_at` lookups for later segments on this line align.
                col += full_len - chars.len();
            }
            y += 1;
            line += 1;
        }

        self.render_popup(area, buffer, ctx);
        if matches!(self.popup, Popup::None) && self.stack_view.is_open() {
            self.stack_view
                .render(area, buffer, ctx.display_name(&self.name));
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
        let sel_addr = g.data.blocks.get(g.sel).map(|b| b.head).unwrap_or(0);
        let info = vec![
            Span::styled(
                format!(" {sel_addr:#x}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  cfg·{}  ", self.code_view.label()),
                Style::default().fg(Color::Blue),
            ),
            Span::styled(
                ctx.display_name(&self.name).to_string(),
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

        let hint =
            " hjkl spatial · b/w·]/[ block · PgUp/Dn panel · Enter read · Space list · i il · v linear · ^O/^F hist · q list";
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
                let box_height = 9u16;
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
                    box_y + 4,
                    format!(
                        "message · {} chars · showing the tail",
                        input.chars().count()
                    ),
                    (box_width - 4) as usize,
                    Style::default().add_modifier(Modifier::DIM),
                );
                let content_width = (box_width - 6) as usize;
                let input_lines = tail_wrapped_lines(input, content_width, 2);
                for (row, line) in input_lines.iter().enumerate() {
                    let prefix = if row == 0 { "> " } else { "  " };
                    crate::ui::put_str(
                        buffer,
                        box_x + 2,
                        box_y + 5 + row as u16,
                        format!("{prefix}{line}"),
                        (box_width - 4) as usize,
                        Style::default(),
                    );
                }
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
                tokens,
                goto,
                off,
                hoff,
                focus,
            } => {
                let box_width = (area.width.saturating_sub(6)).clamp(50, 90);
                let box_height = (area.height.saturating_sub(4)).clamp(8, 22);
                let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
                let box_y = area.y + (area.height.saturating_sub(box_height)) / 2;
                crate::ui::draw_box(buffer, box_x, box_y, box_width, box_height, title);
                let view_height = (box_height - 3) as usize;
                let inner = (box_width - 4) as usize;
                // Any visible line running past the right edge at the current
                // horizontal offset means content is clipped — show the h/l hint.
                let mut clipped = false;
                for (row, line) in lines.iter().skip(*off).take(view_height).enumerate() {
                    let abs = *off + row;
                    let focused = *focus == Some(abs);
                    let y = box_y + 1 + row as u16;
                    // Pan horizontally: drop the first `hoff` chars before clipping.
                    let visible: String = line.chars().skip(*hoff).collect();
                    if visible.chars().count() > inner {
                        clipped = true;
                    }
                    if focused {
                        // A highlight bar across the full inner width for the
                        // focused statement (a decomp peek's use site); its own
                        // white-on-accent styling wins over syntax colours.
                        let style = Style::default()
                            .fg(Color::White)
                            .bg(crate::ui::HILITE_BG)
                            .add_modifier(Modifier::BOLD);
                        crate::ui::put_str(buffer, box_x + 2, y, " ".repeat(inner), inner, style);
                        crate::ui::put_str(buffer, box_x + 2, y, &visible, inner, style);
                    } else if let Some(segs) = tokens.as_ref().and_then(|t| t.get(abs)) {
                        // Syntax-highlight a decompile peek the same way the main
                        // viewer colours pseudo-C.
                        let spans = pan_syntax_spans(segs, *hoff);
                        crate::ui::put_spans(buffer, box_x + 2, y, inner, &spans);
                    } else {
                        crate::ui::put_str(
                            buffer,
                            box_x + 2,
                            y,
                            &visible,
                            inner,
                            Style::default().fg(Color::Yellow),
                        );
                    }
                }
                let hint = match (goto.is_some(), clipped || *hoff > 0) {
                    (true, true) => " j/k scroll · h/l pan · 0 reset · g goto · q close ",
                    (true, false) => " j/k scroll · g goto · ? help · q close ",
                    (false, true) => " j/k scroll · h/l pan · 0 reset · ? help · q close ",
                    (false, false) => " j/k scroll · ? help · q close ",
                };
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + box_height - 1,
                    hint,
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
            Popup::Retype {
                var,
                old_type,
                buf: input,
                checked,
                types,
                sel,
                ..
            } => {
                let suggestions = super::hotspots::type_matches(types, input, 6);
                let sug_rows = suggestions.len() as u16;
                let box_width = (area.width.saturating_sub(6)).clamp(48, 96);
                let box_height = 6 + sug_rows;
                let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
                let box_y = area.y + (area.height.saturating_sub(box_height)) / 2;
                let inner = (box_width - 4) as usize;
                crate::ui::draw_box(
                    buffer,
                    box_x,
                    box_y,
                    box_width,
                    box_height,
                    "retype local  (live in the bn instance)",
                );
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + 1,
                    format!("{var} : {old_type}  →"),
                    inner,
                    Style::default().fg(Color::Cyan),
                );
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + 2,
                    format!("> {input}"),
                    inner,
                    Style::default(),
                );
                let (verdict, vstyle) = match checked {
                    Some(Ok(())) => (
                        format!("✓ valid — Enter applies {} → {}", var, input.trim()),
                        Style::default().fg(Color::Green),
                    ),
                    Some(Err(error)) => (format!("✗ {error}"), Style::default().fg(Color::Red)),
                    None => (
                        "Tab completes · ^P checks the type".to_string(),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                };
                crate::ui::put_str(buffer, box_x + 2, box_y + 3, verdict, inner, vstyle);
                for (row, name) in suggestions.iter().enumerate() {
                    let selected = row == *sel;
                    let style = if selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::Blue)
                    };
                    let marker = if selected { "▸ " } else { "  " };
                    crate::ui::put_str(
                        buffer,
                        box_x + 2,
                        box_y + 4 + row as u16,
                        format!("{marker}{name}"),
                        inner,
                        style,
                    );
                }
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + box_height - 1,
                    " Tab complete · ↑↓ pick · ^P check · Enter apply · Esc cancel ",
                    inner,
                    Style::default().add_modifier(Modifier::DIM),
                );
            }
            Popup::Comment {
                target,
                buf: input,
                cursor,
            } => {
                // Comments can be long (and are pre-filled for edit-in-place), so
                // wrap the value, keep the caret in view, and draw it as a block.
                let box_width = (area.width.saturating_sub(6)).clamp(46, 100);
                let content_width = (box_width as usize).saturating_sub(6).max(1);
                self.comment_wrap.set(content_width);
                let (window, caret_row, caret_col, at_top) =
                    wrap_cursor(input, content_width, 5, *cursor);
                let box_height = 4 + window.len() as u16;
                let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
                let box_y = area.y + (area.height.saturating_sub(box_height)) / 2;
                let inner = (box_width - 4) as usize;
                crate::ui::draw_box(
                    buffer,
                    box_x,
                    box_y,
                    box_width,
                    box_height,
                    "comment  (live in the bn instance)",
                );
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + 1,
                    format!(
                        "comment {}  ·  {} chars",
                        target.label(),
                        input.chars().count()
                    ),
                    inner,
                    Style::default().fg(Color::Cyan),
                );
                for (row, line) in window.iter().enumerate() {
                    let prefix = if row == 0 && at_top { "> " } else { "  " };
                    crate::ui::put_str(
                        buffer,
                        box_x + 2,
                        box_y + 2 + row as u16,
                        format!("{prefix}{line}"),
                        inner,
                        Style::default(),
                    );
                }
                // The caret: reverse-video the char under it (a space at line end).
                let caret_char = input
                    .chars()
                    .nth(*cursor)
                    .filter(|ch| *ch != '\n')
                    .unwrap_or(' ');
                crate::ui::put_str(
                    buffer,
                    box_x + 4 + caret_col as u16,
                    box_y + 2 + caret_row as u16,
                    caret_char.to_string(),
                    1,
                    Style::default().add_modifier(Modifier::REVERSED),
                );
                crate::ui::put_str(
                    buffer,
                    box_x + 2,
                    box_y + box_height - 1,
                    " ←→ move · Home/End · ⌫/Del · Enter set · Esc cancel ",
                    inner,
                    Style::default().add_modifier(Modifier::DIM),
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

#[cfg(test)]
mod tests {
    use super::{pan_syntax_spans, tail_wrapped_lines, wrap_cursor};
    use crate::syntax::{Seg, Tok};

    #[test]
    fn ask_composer_keeps_the_wrapped_tail() {
        assert_eq!(
            tail_wrapped_lines("abcdefghijklmnopqrstuvwxyz", 5, 2),
            vec!["uvwxy", "z"]
        );
        assert_eq!(tail_wrapped_lines("short", 20, 2), vec!["short"]);
    }

    #[test]
    fn wrap_cursor_places_caret_and_windows() {
        // 10 chars at width 4 → rows abcd / efgh / ij; caret index 5 → row 1 col 1.
        let (win, row, col, top) = wrap_cursor("abcdefghij", 4, 5, 5);
        assert_eq!(win, vec!["abcd", "efgh", "ij"]);
        assert_eq!((row, col, top), (1, 1, true));
        // Caret past a full final row opens a fresh row for it.
        let (win, row, col, _top) = wrap_cursor("abcd", 4, 5, 4);
        assert_eq!(win, vec!["abcd", ""]);
        assert_eq!((row, col), (1, 0));
        // Overflow: the window bottom-pins to the caret's row, dropping the top.
        let (win, row, _col, top) = wrap_cursor("aaaabbbbccccdddd", 4, 2, 16);
        assert_eq!(win.len(), 2);
        assert_eq!((row, top), (1, false));
    }

    fn seg(text: &str, kind: Tok) -> Seg {
        Seg {
            text: text.into(),
            kind,
        }
    }

    #[test]
    fn pan_drops_leading_chars_and_splits_mid_token() {
        let line = vec![seg("int ", Tok::Type), seg("x", Tok::Name)];
        // No pan: both segments survive whole.
        let spans = pan_syntax_spans(&line, 0);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "int ");
        assert_eq!(spans[1].content, "x");
        // Pan past the whole first segment (4 chars): only `x` remains.
        let spans = pan_syntax_spans(&line, 4);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "x");
        // Pan into the middle of the first segment: it's clipped, colour kept.
        let spans = pan_syntax_spans(&line, 2);
        assert_eq!(spans[0].content, "t ");
        assert_eq!(spans[1].content, "x");
    }
}
