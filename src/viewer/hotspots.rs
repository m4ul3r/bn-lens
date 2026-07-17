//! Pure helpers for classifying interactive tokens and annotating peek output.

use super::{HotKind, Hotspot};
use crate::ctx::Ctx;
use crate::syntax::{Line, Tok};
use std::collections::HashMap;

/// A valid C identifier: `[A-Za-z_][A-Za-z0-9_]*` (rejects spaces etc.).
pub(super) fn valid_ident(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Truncate `text` to `max_chars` chars with an ellipsis.
pub(super) fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

/// Annotate a hex dump: any 8-byte little-endian value that matches a known
/// symbol address gets a `+off→name` note — so a function-pointer table reads
/// as names, not raw bytes.
pub(super) fn symbolize_dump(dump: &str, reverse_symbols: &HashMap<String, String>) -> Vec<String> {
    dump.lines()
        .map(|line| {
            let after = line.split_once(':').map_or(line, |(_, bytes)| bytes);
            let bytes: Vec<u8> = after
                .split_whitespace()
                .take(16)
                .filter_map(|token| {
                    if token.len() == 2 {
                        u8::from_str_radix(token, 16).ok()
                    } else {
                        None
                    }
                })
                .collect();
            let mut annotation = String::new();
            for offset in [0usize, 8usize] {
                if bytes.len() >= offset + 8 {
                    let mut value: u64 = 0;
                    for i in 0..8 {
                        value |= (bytes[offset + i] as u64) << (8 * i);
                    }
                    if value != 0 {
                        if let Some(name) = reverse_symbols.get(&format!("0x{value:x}")) {
                            if !annotation.is_empty() {
                                annotation.push_str(" ·");
                            }
                            annotation.push_str(&format!(" +{offset:#x}→{name}"));
                        }
                    }
                }
            }
            if annotation.is_empty() {
                line.to_string()
            } else {
                format!("{line}  {annotation}")
            }
        })
        .collect()
}

/// A register-temp local from BN's decompiler (`v0`, `x0_1`, `w19`, `arg3`,
/// `temp2_1`): a register-style prefix followed only by digits, with optional
/// `_N` SSA-suffix parts. These read as spill noise, so Tab skips them — they
/// stay clickable and renamable like any other local.
pub(super) fn is_noise_local(name: &str) -> bool {
    const PREFIXES: [&str; 10] = ["temp", "arg", "v", "w", "x", "q", "d", "s", "h", "b"];
    let Some(rest) = PREFIXES.iter().find_map(|prefix| name.strip_prefix(prefix)) else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split('_')
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

/// Indices of the spans Tab visits: hotspots interesting on their own —
/// functions other than the one in view (its signature name is only a
/// self-goto), data globals, executable addresses, strings, and locals that
/// look deliberately named. Falls back to every span when nothing qualifies,
/// so Tab never goes dead on a view made only of "noise".
pub(super) fn tab_stops(spans: &[Hotspot], viewed: &str) -> Vec<usize> {
    let stops: Vec<usize> = spans
        .iter()
        .enumerate()
        .filter(|(_, span)| match span.kind {
            HotKind::Func => span.target != viewed,
            HotKind::Data | HotKind::Str => true,
            HotKind::Addr => span.code,
            HotKind::Local => !is_noise_local(&span.target),
        })
        .map(|(index, _)| index)
        .collect();
    if stops.is_empty() {
        (0..spans.len()).collect()
    } else {
        stops
    }
}

/// Choose the next Tab landing position within `stop_lines` (the line of each
/// interesting span, in ascending span order; must be non-empty). `active_pos`
/// is the position of the currently Tab/click-selected stop when one is active
/// on the cursor line, else `None`. Stepping from an active stop advances one
/// around the ring; arriving fresh (no active selection — e.g. after a search
/// or a `j`/`k` move) lands on the nearest stop, **preferring the cursor line**
/// (`line >= cline` forward, `line <= cline` back) before wrapping. This is why
/// a `Tab` right after a `/find` selects the match's own hotspot rather than
/// skipping to the next line.
pub(super) fn next_stop(
    stop_lines: &[usize],
    active_pos: Option<usize>,
    cline: usize,
    direction: i32,
) -> usize {
    let count = stop_lines.len() as i64;
    if let Some(at) = active_pos {
        return (at as i64 + direction as i64).rem_euclid(count) as usize;
    }
    if direction > 0 {
        stop_lines
            .iter()
            .position(|&line| line >= cline)
            .unwrap_or(0)
    } else {
        stop_lines
            .iter()
            .rposition(|&line| line <= cline)
            .unwrap_or(stop_lines.len() - 1)
    }
}

/// Is `token` a data global we can resolve/peek? Exports, imported data, any
/// known symbol, or an auto-named `data_<hex>` global.
fn is_data_symbol(ctx: &Ctx, token: &str) -> bool {
    ctx.data_names.contains(token)
        || ctx.addr_by_name.contains_key(token)
        || (ctx.import_names.contains(token) && !ctx.func_names.contains(token))
        || token
            .strip_prefix("data_")
            .is_some_and(|hex| !hex.is_empty() && hex.chars().all(|ch| ch.is_ascii_hexdigit()))
}

/// Promote syntax segments to interactive spans: function names (goto), data
/// globals (peek), and 0x-addresses that land inside a mapped section (peek, or
/// goto if the section is executable). Constants/offsets outside sections stay
/// inert, while known locals and string literals get their own hotspot kinds.
///
/// The function currently in view is promoted too (so its signature name on
/// line 1 is click/`x`/`n`-able); `act_primary` no-ops the self-goto.
pub(super) fn build_spans(
    lines: &[Line],
    ctx: &Ctx,
    locals: &HashMap<String, String>,
) -> Vec<Hotspot> {
    let mut hotspots = Vec::new();
    for (line, segments) in lines.iter().enumerate() {
        let mut col = 0usize;
        for segment in segments {
            let len = segment.text.chars().count();
            match segment.kind {
                Tok::Name => {
                    let (kind, code) = if ctx.func_names.contains(&segment.text) {
                        (Some(HotKind::Func), true)
                    } else if is_data_symbol(ctx, &segment.text) {
                        (Some(HotKind::Data), false)
                    } else if locals.contains_key(&segment.text) {
                        (Some(HotKind::Local), false)
                    } else {
                        (None, false)
                    };
                    if let Some(kind) = kind {
                        hotspots.push(Hotspot {
                            line,
                            col,
                            target: segment.text.clone(),
                            kind,
                            code,
                        });
                    }
                }
                // 0x-address: Num in pseudo-C (tokenize_c), Type in the plain
                // tokenizer used for mlil/disasm/xrefs — accept either.
                Tok::Num | Tok::Type if segment.text.starts_with("0x") => {
                    if let Some((_, _, _, executable)) = crate::ctx::parse_hex(&segment.text)
                        .and_then(|address| ctx.section_of(address))
                    {
                        hotspots.push(Hotspot {
                            line,
                            col,
                            target: segment.text.clone(),
                            kind: HotKind::Addr,
                            code: *executable,
                        });
                    }
                }
                Tok::Str => {
                    // Store content without quotes; this matches the strings map.
                    if let Some(inner) = segment
                        .text
                        .strip_prefix('"')
                        .and_then(|text| text.strip_suffix('"'))
                    {
                        hotspots.push(Hotspot {
                            line,
                            col,
                            target: inner.to_string(),
                            kind: HotKind::Str,
                            code: false,
                        });
                    }
                }
                _ => {}
            }
            col += len;
        }
    }
    hotspots
}

#[cfg(test)]
mod tests {
    use super::{is_noise_local, symbolize_dump, tab_stops, valid_ident, HotKind, Hotspot};
    use std::collections::HashMap;

    fn spot(target: &str, kind: HotKind, code: bool, line: usize) -> Hotspot {
        Hotspot {
            line,
            col: 0,
            target: target.into(),
            kind,
            code,
        }
    }

    #[test]
    fn register_temps_are_noise_named_locals_are_not() {
        // aarch64 register temps and SSA-suffixed spills.
        for name in [
            "v0", "v7", "x0", "x19", "w0", "x0_1", "v7_2", "q3", "arg1", "temp2_1",
        ] {
            assert!(is_noise_local(name), "{name} should be noise");
        }
        // Deliberately-named locals (including ones sharing a register prefix).
        for name in [
            "buf",
            "frame_len",
            "s2_len",
            "dest",
            "var_10",
            "x_coord",
            "value",
        ] {
            assert!(!is_noise_local(name), "{name} should not be noise");
        }
    }

    #[test]
    fn tab_skips_noise_and_the_viewed_functions_own_name() {
        let spans = vec![
            spot("parse_frame", HotKind::Func, true, 0), // signature self-name
            spot("v0", HotKind::Local, false, 2),        // register temp
            spot("frame_len", HotKind::Local, false, 3),
            spot("strcpy", HotKind::Func, true, 4),
            spot("0x1000", HotKind::Addr, false, 5), // non-code address
            spot("0x4010", HotKind::Addr, true, 6),
            spot("boot", HotKind::Str, false, 7),
        ];
        assert_eq!(tab_stops(&spans, "parse_frame"), vec![2, 3, 5, 6]);
    }

    #[test]
    fn tab_from_a_fresh_cursor_prefers_a_stop_on_the_current_line() {
        // Stops sit on lines 2, 5, 5, 8. With the cursor on line 5 and no active
        // selection (the search/j-k case), forward Tab lands on the first stop
        // *on* line 5 — not the next line — which is the /find regression fix.
        let lines = [2usize, 5, 5, 8];
        assert_eq!(super::next_stop(&lines, None, 5, 1), 1);
        // Backward prefers the current line too (its last stop).
        assert_eq!(super::next_stop(&lines, None, 5, -1), 2);
    }

    #[test]
    fn tab_from_an_empty_line_moves_to_the_neighboring_stop() {
        let lines = [2usize, 5, 8];
        // Cursor on line 6 (no stop there): forward → next stop below.
        assert_eq!(super::next_stop(&lines, None, 6, 1), 2);
        // Backward → previous stop above.
        assert_eq!(super::next_stop(&lines, None, 6, -1), 1);
        // Past the last stop wraps to the top; before the first wraps to bottom.
        assert_eq!(super::next_stop(&lines, None, 99, 1), 0);
        assert_eq!(super::next_stop(&lines, None, 0, -1), 2);
    }

    #[test]
    fn tab_from_an_active_selection_steps_the_ring() {
        let lines = [2usize, 5, 5, 8];
        assert_eq!(super::next_stop(&lines, Some(1), 5, 1), 2); // forward
        assert_eq!(super::next_stop(&lines, Some(1), 5, -1), 0); // back
        assert_eq!(super::next_stop(&lines, Some(3), 8, 1), 0); // wrap end→start
        assert_eq!(super::next_stop(&lines, Some(0), 2, -1), 3); // wrap start→end
    }

    #[test]
    fn tab_falls_back_to_everything_when_all_spans_are_noise() {
        let spans = vec![
            spot("v0", HotKind::Local, false, 1),
            spot("x0_1", HotKind::Local, false, 2),
        ];
        assert_eq!(tab_stops(&spans, "sub_1234"), vec![0, 1]);
    }

    #[test]
    fn validates_c_identifiers() {
        assert!(valid_ident("frame_len"));
        assert!(valid_ident("_frame2"));
        assert!(!valid_ident("2frame"));
        assert!(!valid_ident("frame len"));
        assert!(!valid_ident(""));
    }

    #[test]
    fn symbolizes_pointer_in_dump() {
        // A little-endian pointer 0x402760 at offset +0x8.
        let mut reverse_symbols = HashMap::new();
        reverse_symbols.insert("0x402760".to_string(), "handle_inbound_c2_msg".to_string());
        let dump = "00415258: 00 00 00 00 00 00 00 00 60 27 40 00 00 00 00 00  ........";
        let output = symbolize_dump(dump, &reverse_symbols);
        assert!(
            output[0].contains("+0x8→handle_inbound_c2_msg"),
            "got: {}",
            output[0]
        );
    }

    #[test]
    fn leaves_nonmatching_lines_untouched() {
        let reverse_symbols = HashMap::new();
        let dump = "00415258: 01 02 03 04 05 06 07 08 00 00 00 00 00 00 00 00  ........";
        let output = symbolize_dump(dump, &reverse_symbols);
        assert_eq!(output[0], dump);
    }
}
