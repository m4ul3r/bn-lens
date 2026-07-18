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
    Imports,
    Exports,
    Classes,
    Types,
    Marks,
    Viewer,
    Stack,
    Switcher,
}

impl HelpContext {
    fn label(self) -> &'static str {
        match self {
            Self::Picker => "picker",
            Self::Strings => "strings",
            Self::Imports => "imports",
            Self::Exports => "exports",
            Self::Classes => "classes",
            Self::Types => "types",
            Self::Marks => "marks",
            Self::Viewer => "viewer",
            Self::Stack => "stack view",
            Self::Switcher => "switch bn",
        }
    }

    fn section(self) -> &'static str {
        match self {
            Self::Picker => "PICKER",
            Self::Strings => "STRINGS",
            Self::Imports => "IMPORTS",
            Self::Exports => "EXPORTS",
            Self::Classes => "CLASSES",
            Self::Types => "TYPES",
            Self::Marks => "MARKS",
            Self::Viewer => "VIEWER",
            Self::Stack => "STACK VIEW",
            Self::Switcher => "SWITCH BN",
        }
    }

    fn matches(self, scope: &str) -> bool {
        match self {
            Self::Picker => scope.starts_with("PICKER") || scope == "LIST",
            Self::Strings => scope == "STRINGS" || scope == "LIST",
            Self::Imports => scope == "IMPORTS" || scope == "LIST",
            Self::Exports => scope == "EXPORTS" || scope == "LIST",
            Self::Classes => scope == "CLASSES" || scope == "LIST",
            Self::Types => scope.starts_with("TYPES") || scope == "LIST",
            Self::Marks => scope == "MARKS" || scope == "LIST",
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

/// Fixed-width help fields must be truncated before padding. Letting a long
/// key binding spill into the next span made the key and action read as one
/// concatenated string on narrow terminals.
fn help_field(value: &str, width: usize) -> String {
    let value: String = value.chars().take(width).collect();
    format!("{value:<width$}")
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
        scope: "LIST",
        key: "v",
        action: "next list view (symbols→strings→imports→exports→classes→types→marks) — not the IL cycle",
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
        key: "Esc",
        action: "clear filter (never quits)",
    },
    HelpLine::Entry {
        scope: "PICKER",
        key: "q",
        action: "quit the lens",
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
        key: "f",
        action: "toggle format-strings-only (printf-sink surface); ⚠%n marks write primitives",
    },
    HelpLine::Entry {
        scope: "STRINGS",
        key: "p",
        action: "peek uses (exact asm + approximate mapped C at each site)",
    },
    HelpLine::Entry {
        scope: "STRINGS",
        key: "Enter / x",
        action: "open the full xrefs listing",
    },
    HelpLine::Entry {
        scope: "STRINGS",
        key: "m / v / i",
        action: "view menu / cycle view / switch bn",
    },
    HelpLine::Entry {
        scope: "STRINGS",
        key: "Esc / q",
        action: "clear filter, then format filter, then back to symbols / quit",
    },
    HelpLine::Section("IMPORTS"),
    HelpLine::Entry {
        scope: "IMPORTS",
        key: "j/k  ^D/^U  gg/G",
        action: "move / page / ends",
    },
    HelpLine::Entry {
        scope: "IMPORTS",
        key: "f",
        action: "toggle modeled sinks-only (sources remain visible when off)",
    },
    HelpLine::Entry {
        scope: "IMPORTS",
        key: "/",
        action: "filter by name / address / model role",
    },
    HelpLine::Entry {
        scope: "IMPORTS",
        key: "p",
        action: "peek callers (exact asm + approximate mapped C)",
    },
    HelpLine::Entry {
        scope: "IMPORTS",
        key: "Enter / x",
        action: "cross-reference the import (callers)",
    },
    HelpLine::Entry {
        scope: "IMPORTS",
        key: "m / v / i",
        action: "view menu / cycle view / switch bn",
    },
    HelpLine::Entry {
        scope: "IMPORTS",
        key: "Esc / q",
        action: "clear filter/sinks-only, then back to symbols / quit",
    },
    HelpLine::Section("EXPORTS"),
    HelpLine::Entry {
        scope: "EXPORTS",
        key: "j/k  ^D/^U  gg/G",
        action: "move / page / ends",
    },
    HelpLine::Entry {
        scope: "EXPORTS",
        key: "/",
        action: "filter by name / address",
    },
    HelpLine::Entry {
        scope: "EXPORTS",
        key: "p",
        action: "peek uses (exact asm + approximate mapped C)",
    },
    HelpLine::Entry {
        scope: "EXPORTS",
        key: "Enter / x",
        action: "open decompile (function) or xrefs · cross-reference",
    },
    HelpLine::Entry {
        scope: "EXPORTS",
        key: "m / v / i",
        action: "view menu / cycle view / switch bn",
    },
    HelpLine::Entry {
        scope: "EXPORTS",
        key: "Esc / q",
        action: "clear filter, then back to symbols / quit",
    },
    HelpLine::Section("CLASSES"),
    HelpLine::Entry {
        scope: "CLASSES",
        key: "j/k  ^D/^U  gg/G",
        action: "move / page / ends",
    },
    HelpLine::Entry {
        scope: "CLASSES",
        key: "/",
        action: "filter by class / base / confidence",
    },
    HelpLine::Entry {
        scope: "CLASSES",
        key: "Enter / p",
        action: "show RTTI, bases, vtables, methods, and construction evidence",
    },
    HelpLine::Entry {
        scope: "CLASSES",
        key: "m / v / i",
        action: "view menu / cycle view / switch bn",
    },
    HelpLine::Entry {
        scope: "CLASSES",
        key: "Esc / q",
        action: "clear filter, then back to symbols / quit",
    },
    HelpLine::Section("TYPES"),
    HelpLine::Entry {
        scope: "TYPES",
        key: "j/k  ^D/^U  gg/G",
        action: "move / page / ends",
    },
    HelpLine::Entry {
        scope: "TYPES",
        key: "/",
        action: "filter by name / kind",
    },
    HelpLine::Entry {
        scope: "TYPES",
        key: "Enter / p",
        action: "show the type's layout (fields + offsets)",
    },
    HelpLine::Entry {
        scope: "TYPES",
        key: "n",
        action: "new type: write a C declaration and add it",
    },
    HelpLine::Entry {
        scope: "TYPES",
        key: "m / v / i",
        action: "view menu / cycle view / switch bn",
    },
    HelpLine::Entry {
        scope: "TYPES",
        key: "Esc / q",
        action: "clear filter, then back to symbols / quit",
    },
    HelpLine::Section("TYPES EDITOR"),
    HelpLine::Entry {
        scope: "TYPES EDITOR",
        key: "Enter · Tab",
        action: "newline (auto-indent) · insert 4 spaces",
    },
    HelpLine::Entry {
        scope: "TYPES EDITOR",
        key: "^P",
        action: "check — validate the declaration without committing",
    },
    HelpLine::Entry {
        scope: "TYPES EDITOR",
        key: "^S",
        action: "declare — add the type to the live bn instance",
    },
    HelpLine::Entry {
        scope: "TYPES EDITOR",
        key: "Esc",
        action: "cancel",
    },
    HelpLine::Section("MARKS"),
    HelpLine::Entry {
        scope: "MARKS",
        key: "j/k  ^D/^U  gg/G",
        action: "move / page / ends",
    },
    HelpLine::Entry {
        scope: "MARKS",
        key: "/",
        action: "filter by text / address / type / function",
    },
    HelpLine::Entry {
        scope: "MARKS",
        key: "Enter / x",
        action: "open the annotated function / its xrefs",
    },
    HelpLine::Entry {
        scope: "MARKS",
        key: "m / v / i",
        action: "view menu / cycle view / switch bn",
    },
    HelpLine::Entry {
        scope: "MARKS",
        key: "Esc / q",
        action: "clear filter, then back to symbols / quit",
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
        key: "w / b  ·  Tab / Shift-Tab",
        action: "next / previous hotspot (funcs, data, strings, locals including v0_2 temps)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "W / B",
        action: "next / previous call or code target only (skip locals & data)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "g / Enter",
        action: "act on hotspot: goto fn/code · open data in the linear data view (header previews)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "p",
        action: "peek popup — code→decompile, data→quick typed field map",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "(data view)",
        action: "g on data opens a BN-style listing: addresses, data_ labels, hex+ascii, string decls; ^O returns",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "x",
        action: "show xrefs",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "n",
        action: "rename — selected local, else the function in view (imports refused)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "y",
        action: "retype the selected local — autocomplete + preview-validate before commit",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: ";",
        action: "comment — edits the existing one in place (address or function), pre-filled",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "t",
        action: "bookmark (Bookmarks tag)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "i / I",
        action: "cycle IL: decompile → mlil → disasm (also re-renders the cfg in place)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "v",
        action: "toggle cfg ⇄ linear, keeping the IL (list-mode v is different)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "hjkl  (cfg graph)",
        action: "spatial move to nearest block (green=true red=false blue=branch edges)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "w / b · ] / [  (cfg graph)",
        action: "next / previous block in index order (sequential walk)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "PgUp/Dn · ^U/^D  (cfg graph)",
        action: "scroll the top-left block inspector (always shows the highlight)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "Enter/g · Space  (cfg)",
        action: "read the selected block · toggle graph ⇄ list",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "/  then ] / [",
        action: "find; next / previous match",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: ":",
        action: "goto address or symbol (0x… / name; unique name prefix completes) — lands in the containing function, keeps the IL level",
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
        key: "s",
        action: "sections map",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "^O / ^F",
        action: "back / forward in the nav history (function jumplist; not in-view motion)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "q",
        action: "leave to the list now",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "Esc",
        action: "back out one layer: popup → stack → visual → search → history → list",
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
        key: "j/k  PgDn/Up · h/l · 0",
        action: "scroll · pan long lines · reset pan; q/Esc/Enter close",
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
        key: "Enter / n",
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
        key: "/",
        action: "type-ahead filter the focused column (Enter keep · Esc clear)",
    },
    HelpLine::Entry {
        scope: "SWITCH BN",
        key: "Enter",
        action: "select instance and target",
    },
    HelpLine::Entry {
        scope: "SWITCH BN",
        key: "q / Esc",
        action: "q cancels; Esc clears an active filter first, else cancels",
    },
    HelpLine::Section("MOUSE"),
    HelpLine::Entry {
        scope: "PICKER/VIEWER",
        key: "wheel",
        action: "page up/down (same step as PgUp/PgDn)",
    },
    HelpLine::Entry {
        scope: "PICKER/VIEWER",
        key: "click",
        action: "select function / hotspot",
    },
    HelpLine::Entry {
        scope: "CFG GRAPH",
        key: "drag",
        action: "pan the graph canvas (selection unchanged)",
    },
    HelpLine::Entry {
        scope: "CFG GRAPH",
        key: "click · wheel",
        action: "select a block · scroll",
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
        let title = if box_width >= 58 {
            format!("shortcuts · {} · where / key / action", context.label())
        } else {
            format!("shortcuts · {}", context.label())
        };
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
                    crate::ui::put_str(
                        buffer,
                        box_x + 2,
                        y,
                        format!("{marker} {label}"),
                        inner_width,
                        if active { active_style } else { section_style },
                    );
                }
                HelpLine::Entry { scope, key, action } => {
                    let key_style = if context.matches(scope) {
                        active_style
                    } else {
                        Style::default().fg(Color::Green)
                    };
                    if inner_width >= 72 {
                        ui::put_spans(
                            buffer,
                            box_x + 2,
                            y,
                            inner_width,
                            &[
                                Span::styled(format!("{}  ", help_field(scope, 13)), dim),
                                Span::styled(format!("{}  ", help_field(key, 20)), key_style),
                                Span::raw(*action),
                            ],
                        );
                    } else {
                        // Section headers already provide scope at this width;
                        // spend the scarce columns on an unambiguous key/action
                        // split instead of three colliding columns.
                        let key_width = (inner_width / 3).clamp(1, 20);
                        ui::put_spans(
                            buffer,
                            box_x + 2,
                            y,
                            inner_width,
                            &[
                                Span::styled(
                                    format!("{}  ", help_field(key, key_width)),
                                    key_style,
                                ),
                                Span::raw(*action),
                            ],
                        );
                    }
                }
            }
        }

        let position = format!(
            " j/k scroll · g/G ends · ?/q/Esc close   {}/{} ",
            start + 1,
            max_start + 1
        );
        crate::ui::put_str(
            buffer,
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
    use ratatui::buffer::Buffer;

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

    #[test]
    fn help_fields_truncate_before_the_column_separator() {
        assert_eq!(help_field("j/k · Enter · Esc", 8), "j/k · En");
        assert_eq!(format!("{}  action", help_field("?", 4)), "?     action");
    }

    #[test]
    fn narrow_help_keeps_a_visible_key_action_boundary() {
        let mut help = Help::default();
        help.open(HelpContext::Picker);
        let area = Rect::new(0, 0, 54, 16);
        let mut buffer = Buffer::empty(area);
        help.render(area, &mut buffer, HelpContext::Picker);
        let line = (0..area.width)
            .map(|x| buffer[(x, 3)].symbol())
            .collect::<String>();
        let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains("j/k arrows move selection"));
    }
}
