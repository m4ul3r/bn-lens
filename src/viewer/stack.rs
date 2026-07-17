//! Stack-frame model, navigation, and responsive rendering.

use crate::bn::LocalVariable;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

const SIDE_PANEL_MIN_WIDTH: u16 = 120;

pub(super) enum StackAction {
    None,
    Close,
    Jump(String),
    Rename(String),
}

#[derive(Default)]
pub(super) struct StackView {
    open: bool,
    rows: Vec<LocalVariable>,
    selected: usize,
    top: usize,
    non_stack_count: usize,
    screen_rows: Vec<(u16, usize)>,
    rendered_area: Option<Rect>,
    modal: bool,
}

impl StackView {
    pub(super) fn set_locals(&mut self, locals: &[LocalVariable]) {
        self.open = false;
        self.top = 0;
        self.selected = 0;
        self.non_stack_count = locals.iter().filter(|local| !local.is_stack()).count();
        self.rows = locals
            .iter()
            .filter(|local| local.is_stack())
            .cloned()
            .collect();
        self.rows.sort_by(|left, right| {
            right
                .storage
                .cmp(&left.storage)
                .then_with(|| left.is_synthetic().cmp(&right.is_synthetic()))
                .then_with(|| left.name.cmp(&right.name))
        });
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn open(&mut self, preferred_name: Option<&str>) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        self.open = true;
        self.top = 0;
        self.selected = preferred_name
            .and_then(|name| self.rows.iter().position(|local| local.name == name))
            .or_else(|| self.rows.iter().position(|local| !local.is_synthetic()))
            .unwrap_or(0);
        true
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.screen_rows.clear();
        self.rendered_area = None;
    }

    pub(super) fn selected_name(&self) -> Option<&str> {
        self.open
            .then(|| self.rows.get(self.selected))
            .flatten()
            .map(|local| local.name.as_str())
    }

    pub(super) fn select_name(&mut self, name: &str) {
        if let Some(index) = self.rows.iter().position(|local| local.name == name) {
            self.selected = index;
        }
    }

    pub(super) fn rename(&mut self, old: &str, new: &str) {
        for local in &mut self.rows {
            if local.name == old {
                local.name = new.to_string();
            }
        }
    }

    pub(super) fn on_key(&mut self, key: KeyEvent) -> StackAction {
        match key.code {
            KeyCode::Char('S') | KeyCode::Char('q') | KeyCode::Esc => StackAction::Close,
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                StackAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                StackAction::None
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                StackAction::None
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                StackAction::None
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = 0;
                StackAction::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.selected = self.rows.len().saturating_sub(1);
                StackAction::None
            }
            KeyCode::Enter => self
                .rows
                .get(self.selected)
                .map(|local| StackAction::Jump(local.name.clone()))
                .unwrap_or(StackAction::None),
            KeyCode::Char('n') => self
                .rows
                .get(self.selected)
                .filter(|local| !local.is_synthetic())
                .map(|local| StackAction::Rename(local.name.clone()))
                .unwrap_or(StackAction::None),
            _ => StackAction::None,
        }
    }

    /// Handle pointer input when it lands inside the rendered inspector. A
    /// narrow modal consumes all pointer input; a wide side panel leaves code
    /// clicks and scrolling available outside the panel.
    pub(super) fn on_mouse(&mut self, mouse: MouseEvent) -> bool {
        let inside = self
            .rendered_area
            .is_some_and(|area| contains(area, mouse.column, mouse.row));
        if !inside && !self.modal {
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_selection(-3),
            MouseEventKind::ScrollDown => self.move_selection(3),
            MouseEventKind::Down(_) if inside => {
                if let Some((_, index)) = self.screen_rows.iter().find(|(row, _)| *row == mouse.row)
                {
                    self.selected = *index;
                }
            }
            _ => {}
        }
        true
    }

    pub(super) fn panel_rect(&self, area: Rect) -> Option<Rect> {
        if !self.open || area.width < SIDE_PANEL_MIN_WIDTH || area.height < 8 {
            return None;
        }
        let width = (area.width / 3).clamp(42, 54);
        Some(Rect::new(
            area.x + area.width - width,
            area.y + 1,
            width,
            area.height.saturating_sub(2),
        ))
    }

    pub(super) fn is_modal(&self, area: Rect) -> bool {
        self.open && self.panel_rect(area).is_none()
    }

    pub(super) fn render(&mut self, area: Rect, buffer: &mut Buffer, function: &str) {
        if !self.open {
            return;
        }
        let panel = self.panel_rect(area);
        self.modal = panel.is_none();
        let rect = panel.unwrap_or_else(|| modal_rect(area));
        self.rendered_area = Some(rect);
        self.render_box(rect, buffer, function);
    }

    fn render_box(&mut self, area: Rect, buffer: &mut Buffer, function: &str) {
        if area.width < 20 || area.height < 7 {
            return;
        }
        let title = format!("stack · {function}");
        ui::draw_box(buffer, area.x, area.y, area.width, area.height, &title);
        let inner_x = area.x + 2;
        let inner_width = area.width.saturating_sub(4) as usize;
        let recovered = self.recovered_span();
        let summary = format!(
            "↓ high→low · recovered {recovered:#x} · {} stack · {} non-stack",
            self.rows.len(),
            self.non_stack_count
        );
        let summary = ellipsize_to_width(&summary, inner_width);
        crate::ui::put_str(
            buffer,
            inner_x,
            area.y + 1,
            summary,
            inner_width,
            Style::default().fg(Color::Cyan),
        );

        if let Some(local) = self.rows.get(self.selected) {
            let span = local
                .span_to_next
                .map(|value| format!("{value:#x}"))
                .unwrap_or_else(|| "?".into());
            let detail = format!(
                "{} : {} · offset {} · slot {span}",
                local.name,
                local.type_name,
                format_offset(local.storage)
            );
            let detail = ellipsize_to_width(&detail, inner_width);
            crate::ui::put_str(
                buffer,
                inner_x,
                area.y + 2,
                detail,
                inner_width,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        }

        crate::ui::put_str(
            buffer,
            inner_x,
            area.y + 3,
            " offset │ span │ local : type",
            inner_width,
            Style::default().add_modifier(Modifier::DIM),
        );

        let first_row = area.y + 4;
        let view_height = area.height.saturating_sub(5) as usize;
        self.ensure_selected_visible(view_height);
        self.screen_rows.clear();
        for (screen_index, (index, local)) in self
            .rows
            .iter()
            .enumerate()
            .skip(self.top)
            .take(view_height)
            .enumerate()
        {
            let y = first_row + screen_index as u16;
            self.screen_rows.push((y, index));
            let selected = index == self.selected;
            let mut modifiers = Modifier::empty();
            if selected {
                modifiers |= Modifier::REVERSED;
            }
            if local.is_synthetic() {
                modifiers |= Modifier::DIM;
            }
            let offset_style = Style::default().fg(Color::Cyan).add_modifier(modifiers);
            let span_style = Style::default().fg(Color::Green).add_modifier(modifiers);
            let name_style = Style::default()
                .fg(if selected {
                    Color::Yellow
                } else {
                    Color::White
                })
                .add_modifier(modifiers);
            let type_style = Style::default().fg(Color::Gray).add_modifier(modifiers);
            let span = local
                .span_to_next
                .map(|value| format!("{value:#x}"))
                .unwrap_or_else(|| "—".into());
            let alias = index > 0 && self.rows[index - 1].storage == local.storage;
            let name = if alias {
                format!("↳ {}", local.name)
            } else {
                local.name.clone()
            };
            let name_width = if inner_width >= 62 { 18 } else { 10 };
            let offset = ellipsize_to_width(&format_offset(local.storage), 6);
            let span = ellipsize_to_width(&span, 5);
            let name = ellipsize_to_width(&name, name_width);
            // 8 offset + 1 divider + 6 span + 2 divider/padding + name + 2 colon.
            let type_width = inner_width.saturating_sub(19 + name_width);
            let type_name = ellipsize_to_width(&local.type_name, type_width);
            ui::put_spans(
                buffer,
                inner_x,
                y,
                inner_width,
                &[
                    Span::styled(format!(" {offset:>6} "), offset_style),
                    Span::styled("│", offset_style),
                    Span::styled(format!("{span:>5} "), span_style),
                    Span::styled("│ ", span_style),
                    Span::styled(format!("{name:<name_width$}"), name_style),
                    Span::styled(": ", name_style),
                    Span::styled(type_name, type_style),
                ],
            );
        }

        let footer = if area.width >= 70 {
            " j/k slots · Enter jump · n rename · S/q close · ? help "
        } else {
            " j/k · Enter jump · n rename · S/q "
        };
        crate::ui::put_str(
            buffer,
            inner_x,
            area.y + area.height - 1,
            footer,
            inner_width,
            Style::default().add_modifier(Modifier::DIM),
        );
    }

    fn recovered_span(&self) -> u64 {
        self.rows
            .iter()
            .map(|local| local.storage)
            .min()
            .filter(|storage| *storage < 0)
            .map(i64::unsigned_abs)
            .unwrap_or(0)
    }

    fn ensure_selected_visible(&mut self, view_height: usize) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if view_height > 0 && self.selected >= self.top + view_height {
            self.top = self.selected + 1 - view_height;
        }
    }

    fn move_selection(&mut self, delta: i64) {
        if self.rows.is_empty() {
            return;
        }
        self.selected =
            (self.selected as i64 + delta).clamp(0, self.rows.len() as i64 - 1) as usize;
    }
}

fn format_offset(storage: i64) -> String {
    match storage.cmp(&0) {
        std::cmp::Ordering::Less => format!("-{:#x}", storage.unsigned_abs()),
        std::cmp::Ordering::Equal => "0x0".into(),
        std::cmp::Ordering::Greater => format!("+{storage:#x}"),
    }
}

fn ellipsize_to_width(text: &str, width: usize) -> String {
    let length = text.chars().count();
    if length <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut clipped: String = text.chars().take(width - 1).collect();
    clipped.push('…');
    clipped
}

fn modal_rect(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(100);
    let height = area.height.saturating_sub(4).min(30);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.x + area.width && row >= area.y && row < area.y + area.height
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn local(name: &str, storage: i64, span: Option<u64>, source: &str) -> LocalVariable {
        LocalVariable {
            local_id: format!("id:{name}"),
            name: name.into(),
            type_name: "uint8_t[16]".into(),
            source_type: source.into(),
            storage,
            span_to_next: span,
            is_parameter: false,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn orders_stack_high_to_low_and_ignores_register_locals() {
        let mut view = StackView::default();
        view.set_locals(&[
            local("buf", -40, Some(32), "StackVariableSourceType"),
            local("count", 7, None, "RegisterVariableSourceType"),
            local("guard", -8, Some(8), "StackVariableSourceType"),
        ]);
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[0].name, "guard");
        assert_eq!(view.rows[1].name, "buf");
        assert_eq!(view.non_stack_count, 1);
        assert_eq!(view.recovered_span(), 40);
    }

    #[test]
    fn does_not_open_without_stack_backed_locals() {
        let mut view = StackView::default();
        view.set_locals(&[local("count", 7, None, "RegisterVariableSourceType")]);

        assert!(!view.open(None));
        assert!(!view.is_open());
    }

    #[test]
    fn opens_on_preferred_local_and_returns_actions() {
        let mut view = StackView::default();
        view.set_locals(&[
            local("first", -8, Some(8), "StackVariableSourceType"),
            local("second", -16, Some(8), "StackVariableSourceType"),
        ]);
        assert!(view.open(Some("second")));
        assert_eq!(view.selected_name(), Some("second"));
        assert!(matches!(
            view.on_key(key(KeyCode::Enter)),
            StackAction::Jump(name) if name == "second"
        ));
        assert!(matches!(
            view.on_key(key(KeyCode::Char('n'))),
            StackAction::Rename(name) if name == "second"
        ));
        assert!(matches!(
            view.on_key(key(KeyCode::Char('S'))),
            StackAction::Close
        ));
    }

    #[test]
    fn side_panel_is_only_used_when_code_keeps_enough_width() {
        let mut view = StackView::default();
        view.set_locals(&[local("buf", -32, Some(32), "StackVariableSourceType")]);
        view.open(None);
        assert!(view.panel_rect(Rect::new(0, 0, 119, 24)).is_none());
        let panel = view
            .panel_rect(Rect::new(0, 0, 140, 30))
            .expect("wide terminal uses a panel");
        assert!(panel.width >= 42);
        assert!(panel.x >= 80);
    }

    #[test]
    fn formats_signed_binary_ninja_stack_offsets() {
        assert_eq!(format_offset(-40), "-0x28");
        assert_eq!(format_offset(0), "0x0");
        assert_eq!(format_offset(16), "+0x10");
    }

    #[test]
    fn ellipsis_is_only_added_when_text_is_clipped() {
        assert_eq!(ellipsize_to_width("abcdef", 6), "abcdef");
        assert_eq!(ellipsize_to_width("abcdef", 5), "abcd…");
        assert_eq!(ellipsize_to_width("abcdef", 1), "…");
        assert_eq!(ellipsize_to_width("abcdef", 0), "");
    }

    #[test]
    fn rendered_detail_and_type_mark_truncation() {
        let mut view = StackView::default();
        let mut handler = local("handler", -32, Some(32), "StackVariableSourceType");
        handler.type_name =
            "int32_t (*)(std::_Any_data const&, char const*, uint64_t, void*)".into();
        view.set_locals(&[handler]);
        view.open(None);

        let area = Rect::new(0, 0, 140, 30);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer, "main");
        let panel = view.rendered_area.expect("stack panel rendered");
        let line = |y| {
            (panel.x + 2..panel.x + panel.width - 2)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };

        assert!(line(panel.y + 2).ends_with('…'));
        assert!(line(panel.y + 4).ends_with('…'));
    }
}
