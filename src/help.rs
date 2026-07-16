//! Global shortcut reference. `?` opens this above every application mode.

use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

#[derive(Clone, Copy)]
pub enum HelpContext {
    Picker,
    Strings,
    Viewer,
    Stack,
    Switcher,
}

impl HelpContext {
    fn label(self) -> &'static str {
        match self {
            Self::Picker => "picker",
            Self::Strings => "strings",
            Self::Viewer => "viewer",
            Self::Stack => "stack view",
            Self::Switcher => "switch bn",
        }
    }

    fn section(self) -> &'static str {
        match self {
            Self::Picker => "PICKER",
            Self::Strings => "STRINGS",
            Self::Viewer => "VIEWER",
            Self::Stack => "STACK VIEW",
            Self::Switcher => "SWITCH BN",
        }
    }

    fn matches(self, scope: &str) -> bool {
        match self {
            Self::Picker => scope.starts_with("PICKER") || scope == "LIST",
            Self::Strings => scope == "STRINGS" || scope == "LIST",
            Self::Viewer => {
                scope.starts_with("VIEWER") || scope == "VISUAL" || scope == "STACK VIEW"
            }
            Self::Stack => scope == "STACK VIEW",
            Self::Switcher => scope == "SWITCH BN",
        }
    }
}

enum HelpLine {
    Section(&'static str),
    Entry {
        scope: &'static str,
        key: &'static str,
        action: &'static str,
    },
}

const LINES: &[HelpLine] = &[
    HelpLine::Section("GLOBAL"),
    HelpLine::Entry {
        scope: "ANYWHERE",
        key: "?",
        action: "open this shortcut guide",
    },
    HelpLine::Entry {
        scope: "ANYWHERE",
        key: "^R",
        action: "refresh from the live bn instance",
    },
    HelpLine::Entry {
        scope: "LIST",
        key: "m  ·  click title",
        action: "open the bn lens view menu",
    },
    HelpLine::Entry {
        scope: "MENU",
        key: "j/k · Enter · Esc",
        action: "pick view/action · choose · close",
    },
    HelpLine::Entry {
        scope: "HELP",
        key: "j/k  PgDn/Up",
        action: "scroll (g/G jump to ends)",
    },
    HelpLine::Entry {
        scope: "HELP",
        key: "?/q/Esc",
        action: "close help",
    },
    HelpLine::Section("PICKER"),
    HelpLine::Entry {
        scope: "PICKER",
        key: "j/k  arrows",
        action: "move selection",
    },
    HelpLine::Entry {
        scope: "PICKER",
        key: "gg / G",
        action: "first / last function",
    },
    HelpLine::Entry {
        scope: "PICKER",
        key: "^D/^U  PgDn/Up",
        action: "move by page",
    },
    HelpLine::Entry {
        scope: "PICKER",
        key: "/",
        action: "search functions",
    },
    HelpLine::Entry {
        scope: "PICKER",
        key: "Enter / x",
        action: "decompile / show xrefs",
    },
    HelpLine::Entry {
        scope: "PICKER",
        key: "s",
        action: "show sections",
    },
    HelpLine::Entry {
        scope: "PICKER",
        key: "i",
        action: "switch bn instance or target",
    },
    HelpLine::Entry {
        scope: "PICKER",
        key: "q / Esc",
        action: "quit",
    },
    HelpLine::Section("PICKER SEARCH"),
    HelpLine::Entry {
        scope: "PICKER SEARCH",
        key: "type / Bksp",
        action: "edit filter",
    },
    HelpLine::Entry {
        scope: "PICKER SEARCH",
        key: "up / down",
        action: "pick a match",
    },
    HelpLine::Entry {
        scope: "PICKER SEARCH",
        key: "Enter / Tab",
        action: "open match / keep filter",
    },
    HelpLine::Entry {
        scope: "PICKER SEARCH",
        key: "Esc",
        action: "restore previous filter",
    },
    HelpLine::Section("STRINGS"),
    HelpLine::Entry {
        scope: "STRINGS",
        key: "j/k  ^D/^U  gg/G",
        action: "move / page / ends",
    },
    HelpLine::Entry {
        scope: "STRINGS",
        key: "/",
        action: "filter by content or address",
    },
    HelpLine::Entry {
        scope: "STRINGS",
        key: "p",
        action: "peek where it's used (pseudo-C statement at each site)",
    },
    HelpLine::Entry {
        scope: "STRINGS",
        key: "Enter / x",
        action: "open the full xrefs listing",
    },
    HelpLine::Entry {
        scope: "STRINGS",
        key: "m / q",
        action: "view menu / quit",
    },
    HelpLine::Section("VIEWER"),
    HelpLine::Entry {
        scope: "VIEWER",
        key: "j/k  arrows",
        action: "move line cursor",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "^D/^U  PgDn/Up",
        action: "move by page",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "H/Home / G",
        action: "first / last line",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "Tab / Shift-Tab",
        action: "next / previous hotspot",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "g / Enter",
        action: "act on hotspot",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "p / x",
        action: "peek / show xrefs",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "r",
        action: "rename local / function",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: ";",
        action: "comment (address or function)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "t",
        action: "bookmark (Bookmarks tag)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "i / I",
        action: "next / previous code view",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "/  then n/N",
        action: "find; next / previous match",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "V",
        action: "start visual line selection",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "a",
        action: "ask agent about cursor line",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "S",
        action: "open stack-frame inspector",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "s / b",
        action: "sections / back in history",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "q / Esc",
        action: "return to picker",
    },
    HelpLine::Section("VISUAL"),
    HelpLine::Entry {
        scope: "VISUAL",
        key: "j/k  arrows",
        action: "extend selected range",
    },
    HelpLine::Entry {
        scope: "VISUAL",
        key: "a",
        action: "ask agent about selected range",
    },
    HelpLine::Entry {
        scope: "VISUAL",
        key: "q / Esc",
        action: "cancel selection",
    },
    HelpLine::Section("VIEWER INPUT AND POPUPS"),
    HelpLine::Entry {
        scope: "VIEWER SEARCH",
        key: "type / Bksp",
        action: "edit query; Enter find; Esc cancel",
    },
    HelpLine::Entry {
        scope: "ASK PROMPT",
        key: "type / ?",
        action: "compose; ? is literal punctuation",
    },
    HelpLine::Entry {
        scope: "ASK PROMPT",
        key: "Enter / Esc",
        action: "send / cancel",
    },
    HelpLine::Entry {
        scope: "PEEK",
        key: "j/k  PgDn/Up",
        action: "scroll; q/Esc/Enter close",
    },
    HelpLine::Entry {
        scope: "SECTIONS",
        key: "j/k  PgDn/Up",
        action: "scroll; q/Esc/Enter/s close",
    },
    HelpLine::Entry {
        scope: "RENAME",
        key: "type / Bksp",
        action: "edit; Enter rename; Esc cancel",
    },
    HelpLine::Entry {
        scope: "COMMENT",
        key: "type / Bksp",
        action: "edit; Enter set; Esc cancel",
    },
    HelpLine::Entry {
        scope: "BOOKMARK",
        key: "type / Bksp",
        action: "optional note; Enter add; Esc cancel",
    },
    HelpLine::Section("STACK VIEW"),
    HelpLine::Entry {
        scope: "STACK VIEW",
        key: "j/k  PgDn/Up",
        action: "select a recovered stack slot",
    },
    HelpLine::Entry {
        scope: "STACK VIEW",
        key: "Enter / r",
        action: "jump to use / rename local",
    },
    HelpLine::Entry {
        scope: "STACK VIEW",
        key: "S/q/Esc",
        action: "close inspector",
    },
    HelpLine::Section("SWITCH BN"),
    HelpLine::Entry {
        scope: "SWITCH BN",
        key: "j/k  arrows",
        action: "move within column",
    },
    HelpLine::Entry {
        scope: "SWITCH BN",
        key: "h/l  ←/→  Tab",
        action: "change column; Shift-Tab left",
    },
    HelpLine::Entry {
        scope: "SWITCH BN",
        key: "Enter",
        action: "select instance and target",
    },
    HelpLine::Entry {
        scope: "SWITCH BN",
        key: "q / Esc",
        action: "cancel",
    },
    HelpLine::Section("MOUSE"),
    HelpLine::Entry {
        scope: "PICKER/VIEWER",
        key: "wheel",
        action: "scroll",
    },
    HelpLine::Entry {
        scope: "PICKER/VIEWER",
        key: "click",
        action: "select function / hotspot",
    },
];

#[derive(Default)]
pub struct Help {
    open: bool,
    offset: usize,
}

impl Help {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, context: HelpContext) {
        self.open = true;
        self.offset = LINES
            .iter()
            .position(
                |line| matches!(line, HelpLine::Section(label) if *label == context.section()),
            )
            .unwrap_or(0);
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                self.open = false;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.offset = (self.offset + 1).min(LINES.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.offset = self.offset.saturating_sub(1),
            KeyCode::Char('d') | KeyCode::PageDown => {
                self.offset = (self.offset + 10).min(LINES.len().saturating_sub(1));
            }
            KeyCode::Char('u') | KeyCode::PageUp => {
                self.offset = self.offset.saturating_sub(10);
            }
            KeyCode::Char('g') | KeyCode::Home => self.offset = 0,
            KeyCode::Char('G') | KeyCode::End => self.offset = LINES.len().saturating_sub(1),
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.offset = self.offset.saturating_sub(3),
            MouseEventKind::ScrollDown => {
                self.offset = (self.offset + 3).min(LINES.len().saturating_sub(1));
            }
            _ => {}
        }
    }

    pub fn render(&self, area: Rect, buffer: &mut Buffer, context: HelpContext) {
        if !self.open || area.width < 8 || area.height < 5 {
            return;
        }

        let horizontal_margin = if area.width >= 80 { 4 } else { 1 };
        let vertical_margin = if area.height >= 18 { 2 } else { 1 };
        let box_width = area.width.saturating_sub(horizontal_margin * 2);
        let box_height = area.height.saturating_sub(vertical_margin * 2);
        let box_x = area.x + horizontal_margin;
        let box_y = area.y + vertical_margin;
        let title = format!("shortcuts · {} · where / key / action", context.label());
        ui::draw_box(buffer, box_x, box_y, box_width, box_height, &title);

        let inner_width = box_width.saturating_sub(4) as usize;
        let view_height = box_height.saturating_sub(2) as usize;
        let max_start = LINES.len().saturating_sub(view_height);
        let start = self.offset.min(max_start);
        let section_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let active_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().add_modifier(Modifier::DIM);

        for (row, line) in LINES.iter().skip(start).take(view_height).enumerate() {
            let y = box_y + 1 + row as u16;
            match line {
                HelpLine::Section(label) => {
                    let active = *label == context.section();
                    let marker = if active { "●" } else { "─" };
                    buffer.set_stringn(
                        box_x + 2,
                        y,
                        format!("{marker} {label}"),
                        inner_width,
                        if active { active_style } else { section_style },
                    );
                }
                HelpLine::Entry { scope, key, action } => {
                    ui::put_spans(
                        buffer,
                        box_x + 2,
                        y,
                        inner_width,
                        &[
                            Span::styled(format!("{scope:<15}"), dim),
                            Span::styled(
                                format!("{key:<18}"),
                                if context.matches(scope) {
                                    active_style
                                } else {
                                    Style::default().fg(Color::Green)
                                },
                            ),
                            Span::raw(*action),
                        ],
                    );
                }
            }
        }

        let position = format!(
            " j/k scroll · g/G ends · ?/q/Esc close   {}/{} ",
            start + 1,
            max_start + 1
        );
        buffer.set_stringn(
            box_x + 2,
            box_y + box_height - 1,
            position,
            inner_width,
            Style::default().add_modifier(Modifier::DIM),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn help_opens_scrolls_and_closes() {
        let mut help = Help::default();
        help.open(HelpContext::Picker);
        assert!(help.is_open());
        assert!(help.offset > 0);

        let start = help.offset;
        help.on_key(key(KeyCode::Down));
        assert_eq!(help.offset, start + 1);
        help.on_key(key(KeyCode::Char('G')));
        assert_eq!(help.offset, LINES.len() - 1);
        help.on_key(key(KeyCode::Char('g')));
        assert_eq!(help.offset, 0);

        help.on_key(key(KeyCode::Char('?')));
        assert!(!help.is_open());
    }

    #[test]
    fn stack_help_opens_at_stack_section() {
        let mut help = Help::default();
        help.open(HelpContext::Stack);

        assert!(matches!(
            LINES.get(help.offset),
            Some(HelpLine::Section("STACK VIEW"))
        ));
    }
}
