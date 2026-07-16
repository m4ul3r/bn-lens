//! Ratatui rendering for the code viewer and its modal popups.

use super::{HotKind, Popup, View, Viewer};
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
            buffer.set_stringn(
                area.x,
                area.y + 1,
                format!(" /{query}"),
                code_width,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        } else if !self.status.is_empty() {
            buffer.set_stringn(
                area.x,
                area.y + 1,
                &self.status,
                code_width,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            );
        } else if self.vmode {
            buffer.set_stringn(
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
            buffer.set_stringn(area.x, y, gutter, GUTTER_WIDTH as usize, gutter_style);

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
                } else if matches!(self.view, View::Mlil | View::Disasm) {
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
                        buffer.set_stringn(
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
                    buffer.set_stringn(x, y, &chunk, space, style);
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
                buffer.set_stringn(
                    box_x + 2,
                    box_y + 1,
                    destination,
                    (box_width - 4) as usize,
                    Style::default().fg(Color::Yellow),
                );
                buffer.set_stringn(
                    box_x + 2,
                    box_y + 2,
                    label,
                    (box_width - 4) as usize,
                    Style::default().fg(Color::Cyan),
                );
                buffer.set_stringn(
                    box_x + 2,
                    box_y + 3,
                    preview,
                    (box_width - 4) as usize,
                    Style::default().add_modifier(Modifier::DIM),
                );
                buffer.set_stringn(
                    box_x + 2,
                    box_y + 5,
                    format!("> {input}"),
                    (box_width - 4) as usize,
                    Style::default(),
                );
                buffer.set_stringn(
                    box_x + 2,
                    box_y + box_height - 1,
                    " Enter send · Esc cancel ",
                    (box_width - 4) as usize,
                    Style::default().add_modifier(Modifier::DIM),
                );
            }
            Popup::Peek { title, lines, off } => {
                let box_width = (area.width.saturating_sub(6)).clamp(50, 90);
                let box_height = (area.height.saturating_sub(4)).clamp(8, 22);
                let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
                let box_y = area.y + (area.height.saturating_sub(box_height)) / 2;
                crate::ui::draw_box(buffer, box_x, box_y, box_width, box_height, title);
                let view_height = (box_height - 3) as usize;
                for (index, line) in lines.iter().skip(*off).take(view_height).enumerate() {
                    buffer.set_stringn(
                        box_x + 2,
                        box_y + 1 + index as u16,
                        line,
                        (box_width - 4) as usize,
                        Style::default().fg(Color::Yellow),
                    );
                }
                buffer.set_stringn(
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
                buffer.set_stringn(
                    box_x + 2,
                    box_y + 2,
                    format!("{old}  →"),
                    (box_width - 4) as usize,
                    Style::default().fg(Color::Cyan),
                );
                buffer.set_stringn(
                    box_x + 2,
                    box_y + 3,
                    format!("> {input}"),
                    (box_width - 4) as usize,
                    Style::default(),
                );
                if !err.is_empty() {
                    buffer.set_stringn(
                        box_x + 2,
                        box_y + 4,
                        format!("✗ {err}"),
                        (box_width - 4) as usize,
                        Style::default().fg(Color::Red),
                    );
                }
                buffer.set_stringn(
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
        buffer.set_stringn(
            box_x + 2,
            box_y + 2,
            target_line,
            (box_width - 4) as usize,
            Style::default().fg(Color::Cyan),
        );
        buffer.set_stringn(
            box_x + 2,
            box_y + 3,
            format!("> {input}"),
            (box_width - 4) as usize,
            Style::default(),
        );
        buffer.set_stringn(
            box_x + 2,
            box_y + box_height - 1,
            footer,
            (box_width - 4) as usize,
            Style::default().add_modifier(Modifier::DIM),
        );
    }
}
