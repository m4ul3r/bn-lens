//! Pure rendering of a typed data-section map.
//!
//! Binary Ninja types every data location it recognises (a `char`, a `void*`, a
//! `char const (*)[0xc]`, a struct…). This module lays those `DataVar`s out as an
//! aligned field table — address, name, type, and the current value — so a data
//! address reads as a *struct field with context*, not a wall of hex. Pointers
//! are shown resolved to the symbol (or string) they target, and a header marks
//! each section boundary. Pure and unit-tested; the viewer feeds the result into
//! its scrollable peek popup.

use crate::bn::DataVar;
use crate::ctx::parse_hex;
use crate::syntax::{Line, Seg, Tok};

/// A rendered data map: display lines plus the index of the focused variable's
/// row (the address the user acted on), so the popup can centre and highlight it.
pub struct DataMap {
    pub lines: Vec<String>,
    pub focus: Option<usize>,
}

const NAME_W: usize = 24;
const TYPE_W: usize = 22;

/// Truncate `text` to `width` chars (ellipsis when cut), else right-pad to it,
/// so columns stay aligned regardless of the token length.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() > width {
        let head: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        format!("{text:<width$}")
    }
}

/// The display name for a var: its symbol, or a synthesised `data_<hex>` from
/// the address (matching how BN and the viewer name unnamed data).
fn display_name(var: &DataVar) -> String {
    if var.name.is_empty() {
        format!("data_{}", var.addr.trim_start_matches("0x"))
    } else {
        var.name.clone()
    }
}

/// The value column as plain text: a pointer resolved to `→ symbol` /
/// `→ "string"` / `→ 0x…`, or a decoded scalar `= N (0xNN)`, or empty for
/// aggregates/void. The formatting/decode logic lives once, in [`value_segs`];
/// this is that content joined, so the popup map and the linear view can never
/// disagree on a value.
fn value_repr(var: &DataVar) -> String {
    value_segs(var).iter().map(|s| s.text.as_str()).collect()
}

/// Render `vars` (ascending, already windowed) into an aligned field table.
/// `focus_addr` marks the acted-on row; `section_hint` names a var's section
/// when BN didn't attach one. A section header is emitted whenever the section
/// changes, so `.data → .bss` boundaries are visible.
pub fn render(vars: &[DataVar], focus_addr: Option<u64>, section_hint: &str) -> DataMap {
    let mut lines = Vec::new();
    let mut focus = None;
    if vars.is_empty() {
        lines.push(" (no typed data variables in range)".into());
        return DataMap { lines, focus };
    }
    let mut current_section: Option<&str> = None;
    for var in vars {
        let section = if var.section.is_empty() {
            section_hint
        } else {
            var.section.as_str()
        };
        if current_section != Some(section) {
            current_section = Some(section);
            lines.push(format!("── {section} ──"));
        }
        let is_focus = focus_addr.is_some() && parse_hex(&var.addr) == focus_addr;
        let marker = if is_focus { "▸" } else { " " };
        let row = format!(
            "{} {} {}  {}  {}",
            var.addr,
            marker,
            fit(&display_name(var), NAME_W),
            fit(&var.type_name, TYPE_W),
            value_repr(var),
        );
        if is_focus {
            focus = Some(lines.len());
        }
        lines.push(row);
    }
    DataMap { lines, focus }
}

// ── Linear data-section view (Binary Ninja-style) ──────────────────────────

/// A rendered linear data listing plus the line to centre on (the address the
/// user entered on).
pub struct LinearData {
    pub lines: Vec<Line>,
    pub focus: Option<usize>,
}

const HEX_COLS: usize = 16;

fn seg(text: impl Into<String>, kind: Tok) -> Seg {
    Seg {
        text: text.into(),
        kind,
    }
}

/// Parse a `bn read` hexdump (`ADDR: b0 b1 …  ascii`) into a byte buffer indexed
/// from `base`. Gaps are zero-filled so indexing by `addr - base` is always safe.
pub fn parse_hexdump(dump: &str, base: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in dump.lines() {
        let Some((addr_part, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(addr) = parse_hex(addr_part.trim()) else {
            continue;
        };
        let Some(offset) = addr.checked_sub(base) else {
            continue;
        };
        // `bn read` renders `ADDR: hh hh … hh  ascii`, padding the hex field to a
        // fixed width and separating it from the ASCII gutter by two spaces. Cut
        // at that boundary before splitting on whitespace, so a partial trailing
        // row's ASCII (whose 2-char pieces can themselves parse as hex, e.g.
        // "ab") is never mistaken for real bytes.
        let hex_field = rest.split("  ").next().unwrap_or(rest);
        let row: Vec<u8> = hex_field
            .split_whitespace()
            .take(HEX_COLS)
            .filter_map(|token| u8::from_str_radix(token, 16).ok())
            .collect();
        // Place the row at its own offset, zero-filling any gap. Writing by
        // offset (rather than append+truncate) keeps the buffer correct even if
        // bn ever emits a row out of order or repeats one.
        let offset = offset as usize;
        let end = offset + row.len();
        if bytes.len() < end {
            bytes.resize(end, 0);
        }
        bytes[offset..end].copy_from_slice(&row);
    }
    bytes
}

fn byte_at(bytes: &[u8], base: u64, addr: u64) -> Option<u8> {
    addr.checked_sub(base)
        .and_then(|offset| bytes.get(offset as usize))
        .copied()
}

fn printable(byte: u8) -> char {
    if (0x20..=0x7e).contains(&byte) {
        byte as char
    } else {
        '.'
    }
}

/// A C-escaped rendering of the NUL-terminated bytes at `addr` (for an inline
/// `char[]` value), capped so a huge blob can't blow up one line.
fn c_string(bytes: &[u8], base: u64, addr: u64, max_bytes: u64) -> String {
    let mut out = String::new();
    for i in 0..max_bytes {
        match byte_at(bytes, base, addr + i) {
            Some(0) | None => break,
            Some(b'"') => out.push_str("\\\""),
            Some(b'\\') => out.push_str("\\\\"),
            Some(b'\n') => out.push_str("\\n"),
            Some(b'\r') => out.push_str("\\r"),
            Some(b'\t') => out.push_str("\\t"),
            Some(byte) if (0x20..=0x7e).contains(&byte) => out.push(byte as char),
            Some(byte) => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out
}

/// An inline `char[N]` value (the bytes *are* the string), as opposed to a
/// pointer-to-char (`char (*)[N]`, `char*`) which is rendered as a pointer.
fn is_inline_string(var: &DataVar) -> bool {
    var.width > 0
        && var.type_name.contains("char")
        && var.type_name.contains('[')
        && !var.type_name.contains('*')
}

/// The value segments for a var's label: a pointer as `= 0x… → sym`/`→ "str"`
/// (the `0x…` and `sym` kept as their own hotspot-eligible tokens), or a decoded
/// scalar `= N (0xNN)`. Empty for aggregates.
fn value_segs(var: &DataVar) -> Vec<Seg> {
    if let Some(ptr) = &var.ptr {
        let mut segs = vec![seg("= ", Tok::Plain), seg(ptr.clone(), Tok::Num)];
        if let Some(sym) = &var.ptr_sym {
            segs.push(seg(" → ", Tok::Plain));
            segs.push(seg(sym.clone(), Tok::Name));
        } else if let Some(text) = &var.ptr_str {
            segs.push(seg(" → ", Tok::Plain));
            segs.push(seg(format!("\"{text}\""), Tok::Str));
        }
        return segs;
    }
    if let Some(value) = var.value {
        let bits = (var.width.min(8) * 8) as u32;
        let mask = if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        let unsigned = value as u64 & mask;
        return vec![seg(format!("= {unsigned} (0x{unsigned:x})"), Tok::Num)];
    }
    Vec::new()
}

/// A typed label line: `ADDR  name  type  = value`.
fn label_line(var: &DataVar, addr: u64) -> Line {
    let mut segs = vec![
        seg(format!("{addr:08x}"), Tok::Comment),
        seg("  ", Tok::Plain),
        seg(display_name(var), Tok::Name),
    ];
    if !var.type_name.is_empty() {
        segs.push(seg("  ", Tok::Plain));
        segs.push(seg(var.type_name.clone(), Tok::Type));
    }
    let value = value_segs(var);
    if !value.is_empty() {
        segs.push(seg("  ", Tok::Plain));
        segs.extend(value);
    }
    segs
}

/// A `char const NAME[0xN] = "str", 0` declaration for an inline string var.
fn string_decl_line(var: &DataVar, addr: u64, base: u64, bytes: &[u8]) -> Line {
    let text = c_string(bytes, base, addr, var.width);
    vec![
        seg(format!("{addr:08x}"), Tok::Comment),
        seg("  ", Tok::Plain),
        seg("char const ", Tok::Type),
        seg(display_name(var), Tok::Name),
        seg(format!("[{:#x}]", var.width), Tok::Type),
        seg(" = ", Tok::Plain),
        seg(format!("\"{text}\""), Tok::Str),
        seg(", 0", Tok::Plain),
    ]
}

/// Emit `ADDR  hh hh …  |ascii|` rows for `[from, to)` in 16-byte lines. Sets
/// `focus` to the row that spans `focus_addr` (first match wins).
fn emit_hex_rows(
    lines: &mut Vec<Line>,
    focus: &mut Option<usize>,
    from: u64,
    to: u64,
    base: u64,
    bytes: &[u8],
    focus_addr: u64,
) {
    let mut addr = from;
    while addr < to {
        let row_end = (addr + HEX_COLS as u64).min(to);
        let mut hex = String::new();
        let mut ascii = String::new();
        for byte_addr in addr..row_end {
            match byte_at(bytes, base, byte_addr) {
                Some(value) => {
                    hex.push_str(&format!("{value:02x} "));
                    ascii.push(printable(value));
                }
                None => {
                    hex.push_str("?? ");
                    ascii.push('.');
                }
            }
        }
        while hex.chars().count() < HEX_COLS * 3 {
            hex.push(' ');
        }
        if focus.is_none() && addr <= focus_addr && focus_addr < row_end {
            *focus = Some(lines.len());
        }
        lines.push(vec![
            seg(format!("{addr:08x}"), Tok::Comment),
            seg("  ", Tok::Plain),
            seg(hex, Tok::Num),
            seg(" |", Tok::Comment),
            seg(ascii, Tok::Comment),
            seg("|", Tok::Comment),
        ]);
        addr = row_end;
    }
}

/// Render `[base, end)` of a data section as a linear listing: a section header,
/// a typed label (or inline-string declaration) at each data-variable boundary,
/// and hex+ASCII rows for the raw bytes in between — Binary Ninja's data view.
/// `focus_addr` marks the line to centre on.
pub fn linear(
    section_label: &str,
    base: u64,
    end: u64,
    vars: &[DataVar],
    bytes: &[u8],
    focus_addr: u64,
) -> LinearData {
    let mut lines: Vec<Line> = vec![vec![seg(format!("── {section_label} ──"), Tok::Keyword)]];
    let mut focus = None;

    let mut sorted: Vec<(u64, &DataVar)> = vars
        .iter()
        .filter_map(|var| parse_hex(&var.addr).map(|addr| (addr, var)))
        .collect();
    sorted.sort_by_key(|(addr, _)| *addr);

    let mut cursor = base;
    for (addr, var) in sorted {
        if addr < cursor || addr >= end {
            continue; // overlap / out of window
        }
        // Raw bytes before this variable.
        emit_hex_rows(
            &mut lines, &mut focus, cursor, addr, base, bytes, focus_addr,
        );
        // Centre on this var's label when the entered address falls anywhere
        // within its extent, not only exactly at its start: scalar and pointer
        // vars emit no per-byte rows, so an interior focus_addr would otherwise
        // never match and the view would open at the top of the window.
        let extent = (addr + var.width.max(1)).min(end).max(addr + 1);
        if focus.is_none() && addr <= focus_addr && focus_addr < extent {
            focus = Some(lines.len());
        }
        if is_inline_string(var) {
            lines.push(string_decl_line(var, addr, base, bytes));
            cursor = (addr + var.width).min(end);
        } else if var.ptr.is_some() || (var.value.is_some() && var.width > 0) {
            // Scalar/pointer: the label carries the value; no redundant hex.
            lines.push(label_line(var, addr));
            cursor = (addr + var.width.max(1)).min(end);
        } else {
            // Array/struct/unknown: label, then the raw bytes.
            lines.push(label_line(var, addr));
            let var_end = (addr + var.width).min(end).max(addr + 1);
            emit_hex_rows(
                &mut lines, &mut focus, addr, var_end, base, bytes, focus_addr,
            );
            cursor = var_end;
        }
    }
    emit_hex_rows(&mut lines, &mut focus, cursor, end, base, bytes, focus_addr);
    LinearData { lines, focus }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(addr: &str, name: &str, ty: &str, width: u64, section: &str) -> DataVar {
        DataVar {
            addr: addr.into(),
            name: name.into(),
            type_name: ty.into(),
            width,
            value: None,
            ptr: None,
            ptr_sym: None,
            ptr_str: None,
            section: section.into(),
        }
    }

    #[test]
    fn renders_scalar_pointer_and_string_values() {
        let mut level = var("0x482049", "cfg_retries", "char", 1, ".data");
        level.value = Some(2);
        let mut stderr = var("0x482050", "stderr", "uint64_t* const", 8, ".bss");
        stderr.ptr = Some("0x7f00".into());
        stderr.ptr_sym = Some("stderr".into());
        let mut msg = var("0x482010", "", "char const (*)[0xc]", 8, ".data");
        msg.ptr = Some("0x41d000".into());
        msg.ptr_str = Some("scheduler".into());

        let map = render(&[msg, level, stderr], Some(0x482049), ".data");
        let joined = map.lines.join("\n");
        // Scalar decodes to decimal + hex.
        assert!(joined.contains("cfg_retries"));
        assert!(joined.contains("= 2 (0x2)"), "{joined}");
        // Pointer resolves to its symbol.
        assert!(joined.contains("→ stderr"), "{joined}");
        // Unnamed pointer-to-string shows a synth name and the string preview.
        assert!(joined.contains("data_482010"), "{joined}");
        assert!(joined.contains("→ \"scheduler\""), "{joined}");
    }

    #[test]
    fn marks_focus_row_and_section_boundaries() {
        let a = var("0x482048", "cfg_verbose", "char", 1, ".data");
        let b = var("0x482049", "cfg_retries", "char", 1, ".data");
        let c = var("0x482218", "cfg_async", "char", 1, ".bss");
        let map = render(&[a, b, c], Some(0x482049), ".data");
        // Two section headers (.data, .bss).
        assert_eq!(map.lines.iter().filter(|l| l.starts_with("──")).count(), 2);
        // Focus points at the cfg_retries row and carries the marker.
        let focus = map.focus.expect("focus set");
        assert!(map.lines[focus].contains("cfg_retries"));
        assert!(map.lines[focus].contains('▸'));
    }

    #[test]
    fn empty_window_is_reported_not_panicked() {
        let map = render(&[], Some(0x1000), ".data");
        assert_eq!(map.focus, None);
        assert_eq!(map.lines.len(), 1);
        assert!(map.lines[0].contains("no typed data"));
    }

    #[test]
    fn wide_scalar_masks_to_its_width() {
        let mut byte = var("0x1", "b", "char", 1, ".data");
        byte.value = Some(-1); // sign-extended read → 0xff at width 1
        let map = render(&[byte], None, ".data");
        assert!(map.lines[1].contains("= 255 (0xff)"), "{}", map.lines[1]);
    }

    fn text(line: &Line) -> String {
        line.iter().map(|seg| seg.text.as_str()).collect()
    }

    #[test]
    fn hexdump_parses_addressed_rows_into_bytes() {
        let dump = "00482040: 00 00 00 00 00 00 00 00 01 02 00 00 00 00 00 00  ................";
        let bytes = parse_hexdump(dump, 0x482040);
        assert_eq!(bytes.len(), 16);
        assert_eq!(bytes[8], 0x01); // 0x482048
        assert_eq!(bytes[9], 0x02); // 0x482049
    }

    #[test]
    fn linear_lays_out_labels_strings_and_hex() {
        // A pointer scalar, an inline string, and a trailing unknown byte run.
        let mut level = var("0x482049", "cfg_retries", "char", 1, ".data");
        level.value = Some(2);
        let mut msg = var("0x407010", "", "char const [0xb]", 0xb, ".rodata");
        msg.width = 0xb;
        // bytes buffer is indexed from base (0x407010), so the string sits at [0..].
        let mut bytes = vec![0u8; 0x20];
        for (i, b) in b"[%d/%d] > \0".iter().enumerate() {
            bytes[i] = *b;
        }
        let out = linear(
            ".rodata  0x407010–0x407030",
            0x407010,
            0x407030,
            &[msg],
            &bytes,
            0x407010,
        );
        let joined: Vec<String> = out.lines.iter().map(text).collect();
        // Section header first.
        assert!(joined[0].contains("── .rodata"));
        // Inline string rendered as a C declaration.
        assert!(
            joined
                .iter()
                .any(|l| l.contains("char const data_407010[0xb] = \"[%d/%d] > \"")),
            "{joined:?}"
        );
        // Focus points at the string's line.
        assert!(out.focus.is_some());
        assert!(text(&out.lines[out.focus.unwrap()]).contains("data_407010"));

        // A scalar renders as a typed label with its value, no hex row.
        let out2 = linear(".data", 0x482049, 0x48204a, &[level], &[2u8], 0x482049);
        let joined2: Vec<String> = out2.lines.iter().map(text).collect();
        assert!(
            joined2
                .iter()
                .any(|l| l.contains("cfg_retries") && l.contains("= 2 (0x2)")),
            "{joined2:?}"
        );
    }

    #[test]
    fn linear_emits_hex_rows_for_unknown_bytes() {
        let bytes: Vec<u8> = (0..16).collect();
        let out = linear(".data", 0x1000, 0x1010, &[], &bytes, 0x1000);
        let joined: Vec<String> = out.lines.iter().map(text).collect();
        // A hex row with the ASCII gutter for the raw bytes.
        assert!(
            joined
                .iter()
                .any(|l| l.contains("00 01 02 03") && l.contains('|')),
            "{joined:?}"
        );
    }

    #[test]
    fn hexdump_ignores_ascii_gutter_on_a_partial_row() {
        // A short trailing row whose ASCII ("ab") is itself valid hex must not
        // be parsed as a phantom 0xab byte after the real data.
        assert_eq!(
            parse_hexdump("00482040: 61 62  ab", 0x482040),
            vec![0x61, 0x62]
        );
        // Same with the realistic padded hex field before the two-space gutter.
        let padded = "00482040: 61 62                                             ab";
        assert_eq!(parse_hexdump(padded, 0x482040), vec![0x61, 0x62]);
        // Multi-token ASCII ("ab cd") is likewise excluded.
        assert_eq!(
            parse_hexdump("00482040: 61 62 20 63 64  ab cd", 0x482040),
            vec![0x61, 0x62, 0x20, 0x63, 0x64]
        );
    }

    #[test]
    fn interior_address_of_a_scalar_var_centres_on_its_label() {
        // An 8-byte pointer var at 0x1000; entering at 0x1004 (interior — the
        // pointer emits no per-byte rows) must still centre on the var's label
        // instead of falling back to line 0.
        let mut ptr = var("0x1000", "g_handler", "void*", 8, ".data");
        ptr.ptr = Some("0x401230".into());
        ptr.ptr_sym = Some("handler_a".into());
        let out = linear(".data", 0x1000, 0x1040, &[ptr], &[], 0x1004);
        let focus = out.focus.expect("interior address focuses the var");
        assert!(
            text(&out.lines[focus]).contains("g_handler"),
            "{:?}",
            text(&out.lines[focus])
        );
    }
}
