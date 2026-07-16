//! Colour mapping for syntax tokens (kept out of the tokenizer).

use crate::syntax::Tok;
use ratatui::style::{Color, Modifier, Style};

pub fn tok_style(tok: Tok) -> Style {
    let c = match tok {
        Tok::Comment => return Style::default().fg(Color::Green).add_modifier(Modifier::DIM),
        Tok::Keyword => Color::Magenta,
        Tok::Type => Color::Cyan,
        Tok::Str => Color::Yellow,
        Tok::Num => Color::Yellow,
        Tok::Name => Color::Blue,
        Tok::Plain => Color::Reset,
    };
    Style::default().fg(c)
}

/// Palette for mlil/disasm (the pseudo-C one rainbows bare hex). Addresses/bytes
/// recede (dim), `0x…` reads cyan, everything else — mnemonics, registers,
/// operators — stays plain; hotspots layer their own colour on top.
pub fn asm_style(tok: Tok) -> Style {
    match tok {
        Tok::Type => Style::default().fg(Color::Cyan),                 // 0x… addresses
        Tok::Num => Style::default().add_modifier(Modifier::DIM),      // bare hex: addr col, bytes, imms
        Tok::Str => Style::default().fg(Color::Yellow),
        Tok::Comment => Style::default().fg(Color::Green).add_modifier(Modifier::DIM),
        _ => Style::default(),                                         // mnemonics / registers / ops
    }
}

pub const FUNC: Color = Color::Blue; // callee target
pub const DATA: Color = Color::Cyan; // data-global target
pub const ADDR: Color = Color::Cyan;
pub const NAME: Color = Color::Reset;
pub const MARK: Color = Color::Yellow; // ● visible marker
