//! Pure helpers for mapping a use address back to a pseudo-C statement, built
//! on `bn decompile --addresses` (each source line prefixed by its 8-hex
//! address column).
//!
//! Shared by the Strings "usage" popup and the viewer's decomp peek of a code
//! hotspot, so the address→statement mapping is defined and tested once.

/// One rendered decompile line: its instruction address (`None` for blank
/// separators) and the pseudo-C with the address column removed and indentation
/// normalized (see [`dec_lines`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecLine {
    pub addr: Option<u64>,
    pub text: String,
}

/// Parse `bn decompile --addresses` text into display lines. Each real line
/// starts with an 8-hex address column; blank separators are kept (so the peek
/// reads like the decompiler), the interior-address `// bn:` resolver note is
/// dropped, and the common leading whitespace the address layout adds is removed
/// so relative nesting is preserved without the base gap.
pub fn dec_lines(text: &str) -> Vec<DecLine> {
    let mut rows: Vec<(Option<u64>, String)> = Vec::new();
    for line in text.lines() {
        // Test the 8-byte address column via bytes so we never `split_at` on a
        // non-char-boundary (a comment line with a multibyte char at index 8
        // would otherwise panic). ASCII-hex first 8 bytes ⇒ byte 8 is a
        // boundary, so the `[..8]`/`[8..]` slices below are safe.
        let bytes = line.as_bytes();
        if bytes.len() >= 8 && bytes[..8].iter().all(u8::is_ascii_hexdigit) {
            if let Ok(addr) = u64::from_str_radix(&line[..8], 16) {
                rows.push((Some(addr), line[8..].trim_end().to_string()));
                continue;
            }
        }
        let trimmed = line.trim_end();
        // The only address-less line bn adds is its interior-resolution note.
        if trimmed.trim_start().starts_with("// bn:") {
            continue;
        }
        rows.push((None, trimmed.to_string()));
    }

    // Common indent over non-blank bodies (leading whitespace is spaces, so byte
    // count == column count and slicing is char-safe).
    let indent = rows
        .iter()
        .filter(|(_, t)| !t.trim().is_empty())
        .map(|(_, t)| t.len() - t.trim_start().len())
        .min()
        .unwrap_or(0);

    rows.into_iter()
        .map(|(addr, t)| {
            let text = if t.len() >= indent {
                t[indent..].to_string()
            } else {
                t
            };
            DecLine { addr, text }
        })
        .collect()
}

/// The addresses of every real (address-bearing) statement line, in order.
pub fn line_addrs(dec: &[DecLine]) -> Vec<u64> {
    dec.iter().filter_map(|l| l.addr).collect()
}

/// `(address, trimmed pseudo-C)` for every real line — the compact form the
/// strings usage popup renders and maps callsites against.
pub fn addr_lines(text: &str) -> Vec<(u64, String)> {
    dec_lines(text)
        .into_iter()
        .filter_map(|l| l.addr.map(|a| (a, l.text.trim().to_string())))
        .filter(|(_, t)| !t.is_empty())
        .collect()
}

/// The address key of the statement covering `site`: an exact match if present,
/// else the greatest address at/below `site` (the instruction falls inside that
/// statement). `None` when nothing is at/below `site`.
pub fn resolve_stmt_addr(addrs: &[u64], site: u64) -> Option<u64> {
    if addrs.contains(&site) {
        return Some(site);
    }
    addrs.iter().copied().filter(|a| *a <= site).max()
}

/// The pseudo-C line(s) of the statement covering `site`. Empty when nothing is
/// at/below `site`.
pub fn lines_at(dec: &[(u64, String)], site: u64) -> Vec<&str> {
    let addrs: Vec<u64> = dec.iter().map(|(a, _)| *a).collect();
    match resolve_stmt_addr(&addrs, site) {
        Some(addr) => dec
            .iter()
            .filter(|(a, _)| *a == addr)
            .map(|(_, t)| t.as_str())
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{addr_lines, dec_lines, line_addrs, lines_at, resolve_stmt_addr};

    // 8-hex address column, an 8-space base gap, +4 per nesting level.
    const DEC: &str = "\
0040428c        void* sub_40428c(void* arg1)
0040428c        {
004042bc            size_t x0_1 = strlen(arg4);

004042cc            if (x20_2)
004042d0                x19 = x0_1 + 1;
00404304            /* tailcall */
00404304            return memcpy(result, arg4, x19);
0040428c        }";

    #[test]
    fn dec_lines_normalizes_indent_and_keeps_blanks() {
        let dec = dec_lines(DEC);
        // the base gap (signature/braces column) de-indents to column 0
        assert_eq!(dec[0].text, "void* sub_40428c(void* arg1)");
        assert_eq!(dec[1].text, "{");
        // function-body nesting is preserved relative to that: +4 per level
        assert_eq!(dec[4].text, "    if (x20_2)");
        assert_eq!(dec[5].text, "        x19 = x0_1 + 1;");
        // blank separator kept, with no address
        assert!(dec.iter().any(|l| l.addr.is_none() && l.text.is_empty()));
    }

    #[test]
    fn dec_lines_does_not_panic_on_multibyte_at_the_address_column() {
        // A comment line ≥8 bytes with a multibyte char straddling byte 8 must
        // not be split_at(8)'d — it's address-less, kept as-is.
        let dec = dec_lines("/* café ☕ */\n0040428c        x = 1;");
        assert_eq!(dec[0].addr, None);
        assert!(dec[0].text.contains("café"));
        assert_eq!(dec[1].addr, Some(0x40428c));
    }

    #[test]
    fn dec_lines_drops_the_bn_resolver_note() {
        let dec = dec_lines(&format!("// bn: 0x404304 is inside sub_40428c\n{DEC}"));
        assert!(!dec.iter().any(|l| l.text.starts_with("// bn")));
    }

    #[test]
    fn maps_callsite_to_pseudo_c_statement() {
        let dec = addr_lines(DEC);
        assert_eq!(
            lines_at(&dec, 0x404304),
            vec!["/* tailcall */", "return memcpy(result, arg4, x19);"]
        );
        // an address inside a statement snaps to the greatest addr at/below it
        assert_eq!(
            lines_at(&dec, 0x4042c0),
            vec!["size_t x0_1 = strlen(arg4);"]
        );
        assert!(lines_at(&dec, 0x400000).is_empty());
    }

    #[test]
    fn resolve_prefers_exact_then_nearest_below() {
        let addrs = line_addrs(&dec_lines(DEC));
        assert_eq!(resolve_stmt_addr(&addrs, 0x404304), Some(0x404304));
        assert_eq!(resolve_stmt_addr(&addrs, 0x4042c0), Some(0x4042bc));
        assert_eq!(resolve_stmt_addr(&addrs, 0x400000), None);
    }
}
