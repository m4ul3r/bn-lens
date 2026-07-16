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
pub(super) fn build_spans(
    lines: &[Line],
    ctx: &Ctx,
    locals: &HashMap<String, String>,
    current: Option<&str>,
) -> Vec<Hotspot> {
    let mut hotspots = Vec::new();
    for (line, segments) in lines.iter().enumerate() {
        let mut col = 0usize;
        for segment in segments {
            let len = segment.text.chars().count();
            match segment.kind {
                Tok::Name if Some(segment.text.as_str()) != current => {
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
    use super::{symbolize_dump, valid_ident};
    use std::collections::HashMap;

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
