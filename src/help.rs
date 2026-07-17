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
        action: "next list view (symbols→strings→imports→exports→types→marks) — not the IL cycle",
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
        key: "m / v / i",
        action: "view menu / cycle view / switch bn",
    },
    HelpLine::Entry {
        scope: "STRINGS",
        key: "Esc / q",
        action: "clear filter, then back to symbols / quit",
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
        action: "toggle sinks-only (⚠ buffer/command, • format/source)",
    },
    HelpLine::Entry {
        scope: "IMPORTS",
        key: "/",
        action: "filter by name / address / category",
    },
    HelpLine::Entry {
        scope: "IMPORTS",
        key: "p",
        action: "peek callers (pseudo-C at each callsite)",
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
        action: "peek uses (pseudo-C at each callsite)",
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
        key: "Tab / Shift-Tab",
        action: "next / previous interesting hotspot (register temps skipped; click reaches them)",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "g / Enter",
        action: "act on hotspot",
    },
    HelpLine::Entry {
        scope: "VIEWER",
        key: "p",
        action: "peek — code→decompile, data→bytes",
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
        key: "] / [  (cfg graph)",
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
        key: "b / w",
        action: "back / forward in the nav history",
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
        action: "scroll",
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
