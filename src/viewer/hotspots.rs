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

/// How to read a pointer-sized word out of a raw dump row: the target's pointer
/// width in bytes and its byte order.
///
/// The typed data map gets the same fact from BN directly (`DATA_MAP_PROGRAM`'s
/// `psz = bv.address_size`, evaluated inside the bridge), so the two can only
/// diverge in one direction: this side may decline to decode where the data map
/// still resolves pointers. That is deliberate — the data map holds the
/// authoritative value, and the raw peek would be *guessing* one.
///
/// There is deliberately **no `Default`**. The old default (64-bit
/// little-endian) is a guess about the target, and a misdecoded word does not
/// merely fail to match the symbol map: it can collide with a real reverse-symbol
/// address and print a confident wrong name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PtrFmt {
    pub width: usize,
    pub big_endian: bool,
}

/// The target's pointer format, or `None` when it is not known.
///
/// Resolution order:
///
/// 1. **What BN itself reported** — `Ctx::ptr_size` / `Ctx::big_endian`, i.e.
///    `bv.address_size` and `bv.endianness` as carried by `bn target info`.
///    Authoritative for *any* architecture, including custom ones. (Today's bn
///    does not put these on the `target_info` payload — verified against
///    `bridge.py::_target_info`, which sends `arch`/`platform`/`image_base`/… —
///    so this tier activates when a bn that does ships. The fields decode as
///    absent until then, they are not assumed.)
/// 2. [`arch_ptr_fmt`], the architecture-*name* table below: a legacy fallback
///    covering the names BN ships, for bns that report nothing.
/// 3. `None` — annotate nothing. Assuming 64-bit little-endian for every
///    architecture outside the table (SPARC, anything custom) is what produced
///    the wrong-name class above; a missing annotation is recoverable, a
///    fabricated `→ name` on a firmware target is not.
pub(super) fn ptr_fmt(ctx: &Ctx) -> Option<PtrFmt> {
    resolve_ptr_fmt(ctx.ptr_size, ctx.big_endian, &ctx.arch)
}

/// [`ptr_fmt`] over the three facts it reads, so the resolution order is testable
/// without a live `Ctx`.
pub(super) fn resolve_ptr_fmt(
    ptr_size: Option<usize>,
    big_endian: Option<bool>,
    arch: &str,
) -> Option<PtrFmt> {
    match (ptr_size, big_endian) {
        (Some(width), Some(big_endian)) if (1..=8).contains(&width) => {
            Some(PtrFmt { width, big_endian })
        }
        _ => arch_ptr_fmt(arch),
    }
}

/// The pointer format of a Binary Ninja architecture *name* (`Ctx::arch`, which
/// is `str(bv.arch)`: `x86_64`, `armv7`, `mipsel32`, `ppc64_le`, …), or `None`
/// for a name not in the table.
///
/// Endianness comes from BN's own naming convention: the big-endian
/// architectures are the unsuffixed MIPS/PPC names plus the explicit `eb` ARM
/// variants, and every little-endian counterpart is spelled out (`mipsel32`,
/// `ppc_le`). This is a *fallback* for bns that don't report the authoritative
/// fields — see [`ptr_fmt`] — and it is exhaustive by construction: an
/// unrecognised name yields `None`, never a guess.
pub(super) fn arch_ptr_fmt(arch: &str) -> Option<PtrFmt> {
    let name = arch.trim().to_ascii_lowercase();
    let width = match name.as_str() {
        "x86_64" | "aarch64" | "aarch64eb" | "arm64" | "mips64" | "mipsel64" | "cavium-mips64"
        | "cavium-mipsel64" | "ppc64" | "ppc64_le" | "rv64gc" | "riscv64" => 8,
        "x86" | "i386" | "armv7" | "armv7eb" | "thumb2" | "thumb2eb" | "mips32" | "mipsel32"
        | "ppc" | "ppc_le" | "ppc_ps" | "ppc_qpx" | "rv32gc" | "rv32gc_wch" | "riscv32" => 4,
        "x86_16" | "msp430" | "z80" | "6502" => 2,
        _ => return None,
    };
    let big_endian = matches!(
        name.as_str(),
        "aarch64eb"
            | "armv7eb"
            | "thumb2eb"
            | "mips32"
            | "mips64"
            | "cavium-mips64"
            | "ppc"
            | "ppc64"
            | "ppc_ps"
            | "ppc_qpx"
    );
    Some(PtrFmt { width, big_endian })
}

/// Decode `bytes` as one word in `fmt`'s byte order.
fn decode_word(bytes: &[u8], big_endian: bool) -> u64 {
    let mut value: u64 = 0;
    if big_endian {
        for &byte in bytes {
            value = (value << 8) | byte as u64;
        }
    } else {
        for (index, &byte) in bytes.iter().enumerate() {
            value |= (byte as u64) << (8 * index);
        }
    }
    value
}

/// Annotate a hex dump: any pointer-sized word (per `fmt`, from the target's own
/// metadata — **not** a hardcoded 64-bit little-endian read) that matches a known
/// symbol address gets a `+off→name` note, so a function-pointer table or vtable
/// reads as names, not raw bytes. Each row is stepped at the pointer stride, so a
/// 32-bit target annotates `+0x0`/`+0x4`/`+0x8`/`+0xc` where a 64-bit one
/// annotates `+0x0`/`+0x8`.
///
/// `fmt: None` (the target's pointer format is unknown — see [`ptr_fmt`]) returns
/// the dump verbatim. Guessing a width would let a misdecoded word collide with a
/// real symbol address and print a name that is simply wrong.
pub(super) fn symbolize_dump(
    dump: &str,
    reverse_symbols: &HashMap<String, String>,
    fmt: Option<PtrFmt>,
) -> Vec<String> {
    let Some(fmt) = fmt else {
        return dump.lines().map(str::to_string).collect();
    };
    let width = fmt.width.clamp(1, 8);
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
            for offset in (0..bytes.len()).step_by(width) {
                let Some(word) = bytes.get(offset..offset + width) else {
                    break;
                };
                let value = decode_word(word, fmt.big_endian);
                if value != 0 {
                    if let Some(name) = reverse_symbols.get(&format!("0x{value:x}")) {
                        if !annotation.is_empty() {
                            annotation.push_str(" ·");
                        }
                        annotation.push_str(&format!(" +{offset:#x}→{name}"));
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

/// Indices of the spans `w`/`b`/Tab visit: functions other than the one in
/// view (signature self-name is only a self-goto), data globals, **every**
/// in-section address — code (a jump/branch target) *and* data (a bare `.bss`/
/// `.data` pointer you can open in the data view) — strings, and every local,
/// including BN register temps (`v0_2`, `x0_1`). Those temps look like spill
/// noise but are the first thing you rename while cleaning a decompile; skipping
/// them made `w` jump past whole argument lists. A bare data address like
/// `0x6f9dc0` is just as actionable as a named global (`g` opens the data view,
/// `p` peeks) — only the call ring [`call_stops`] excludes it. Falls back to
/// every span when nothing qualifies.
pub(super) fn tab_stops(spans: &[Hotspot], viewed: &str) -> Vec<usize> {
    let stops: Vec<usize> = spans
        .iter()
        .enumerate()
        .filter(|(_, span)| match span.kind {
            HotKind::Func => span.target != viewed,
            HotKind::Data | HotKind::Str | HotKind::Local | HotKind::Addr => true,
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

/// The subset of tab stops that are **call/jump targets** — functions other
/// than the one in view, plus executable addresses. `W`/`B` step only these so
/// following control flow doesn't stop on every local or data reference. Unlike
/// [`tab_stops`] there is no fall-back: with no calls in view the caller reports
/// "no calls" rather than looping the full ring.
pub(super) fn call_stops(spans: &[Hotspot], viewed: &str) -> Vec<usize> {
    spans
        .iter()
        .enumerate()
        .filter(|(_, span)| match span.kind {
            HotKind::Func => span.target != viewed,
            HotKind::Addr => span.code,
            HotKind::Data | HotKind::Str | HotKind::Local => false,
        })
        .map(|(index, _)| index)
        .collect()
}

/// The span (if any) whose displayed extent covers character column `col` on
/// `line`. Lets a `/find` land its selection on the **matched token** — so `g`
/// follows the call you searched for, not the line's leftmost hotspot. A `Str`
/// span's display includes its surrounding quotes (the stored target does not);
/// every other kind spans exactly its identifier/address text.
pub(super) fn covering_span(spans: &[Hotspot], line: usize, col: usize) -> Option<usize> {
    spans.iter().position(|span| {
        if span.line != line {
            return false;
        }
        let display_len = match span.kind {
            HotKind::Str => span.target.chars().count() + 2,
            _ => span.target.chars().count(),
        }
        .max(1);
        span.col <= col && col < span.col + display_len
    })
}

/// Ellipsize a string-literal *segment* (`"…"`, quotes included) to about
/// `limit` characters for inline display, keeping the opening quote, a prefix of
/// the content, `…`, and the closing quote (`"long conte…"`). Returns `None`
/// when it already fits. The hotspot keeps the full content, so peek/xref still
/// resolve — only the drawn width shrinks, so a boilerplate string can't eat
/// several wrapped rows of real code.
pub(super) fn truncate_str_segment(text: &str, limit: usize) -> Option<String> {
    if limit == 0 || text.chars().count() <= limit {
        return None;
    }
    let closing = if text.ends_with('"') { "\"" } else { "" };
    let head: String = text.chars().take(limit.saturating_sub(1)).collect();
    Some(format!("{head}…{closing}"))
}

/// Common C primitive/pointer types always offered for retype autocomplete,
/// on top of the target's own declared types — so `char*`, `uint32_t`, etc. are
/// suggestible even when a target never declared them as named types.
pub(super) const BUILTIN_TYPES: &[&str] = &[
    "void",
    "bool",
    "char",
    "signed char",
    "unsigned char",
    "short",
    "unsigned short",
    "int",
    "unsigned int",
    "long",
    "unsigned long",
    "long long",
    "float",
    "double",
    "int8_t",
    "int16_t",
    "int32_t",
    "int64_t",
    "uint8_t",
    "uint16_t",
    "uint32_t",
    "uint64_t",
    "size_t",
    "ssize_t",
    "intptr_t",
    "uintptr_t",
    "void*",
    "char*",
    "const char*",
    "unsigned char*",
    "int*",
    "uint8_t*",
    "uint32_t*",
    "uint64_t*",
    "void**",
];

/// Autocomplete candidates for a retype: type names from `types` (already
/// deduped) that match `query`, ranked exact → prefix → substring, then by
/// length, then alphabetically. Empty query yields nothing (no noise before the
/// user types). Case-insensitive; capped at `limit`.
pub(super) fn type_matches(types: &[String], query: &str, limit: usize) -> Vec<String> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u8, usize, &str)> = types
        .iter()
        .filter_map(|name| {
            let lower = name.to_lowercase();
            let rank = if lower == q {
                0
            } else if lower.starts_with(&q) {
                1
            } else if lower.contains(&q) {
                2
            } else {
                return None;
            };
            Some((rank, name.chars().count(), name.as_str()))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, name)| name.to_string())
        .collect()
}

/// The address encoded in an auto-named data global (`data_<hex>` → `0x<hex>`).
/// BN invents these synthetic names for unnamed data locations; they are *not*
/// bn-resolvable symbols (`bn xrefs data_482049` fails), so peek/xref must
/// recover the address from the name. `None` for anything that isn't a
/// `data_<hex>` name.
pub(super) fn data_symbol_addr(name: &str) -> Option<String> {
    let hex = name.strip_prefix("data_")?;
    (!hex.is_empty() && hex.chars().all(|ch| ch.is_ascii_hexdigit())).then(|| format!("0x{hex}"))
}

/// Is `token` a data global we can resolve/peek? Exports, imported data, any
/// known symbol, or an auto-named `data_<hex>` global.
fn is_data_symbol(ctx: &Ctx, token: &str) -> bool {
    ctx.data_names.contains(token)
        || ctx.addr_by_name.contains_key(token)
        || (ctx.import_names.contains(token) && !ctx.func_names.contains(token))
        || data_symbol_addr(token).is_some()
}

/// Promote syntax segments to interactive spans: function names (goto), data
/// globals (peek), and 0x-addresses that land inside a mapped section (peek, or
/// goto if the section is executable). Constants/offsets outside sections stay
/// inert, while known locals and string literals get their own hotspot kinds.
///
/// The function currently in view is promoted too (so its signature name on
/// line 1 is click/`x`/`n`-able); `act_primary` no-ops the self-goto.
/// Decide a `Tok::Name`'s hotspot kind from what the name is known to be, in
/// strict precedence: a function name wins first; then a name in the function's
/// own locals map — a local must beat a same-named *global* data symbol, or `n`
/// on the token falls through to renaming the whole function and `g` jumps to an
/// unrelated global; then a data symbol; else it's not a hotspot.
fn classify_name(is_func: bool, is_local: bool, is_data: bool) -> Option<(HotKind, bool)> {
    if is_func {
        Some((HotKind::Func, true))
    } else if is_local {
        Some((HotKind::Local, false))
    } else if is_data {
        Some((HotKind::Data, false))
    } else {
        None
    }
}

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
                    if let Some((kind, code)) = classify_name(
                        ctx.func_names.contains(&segment.text),
                        locals.contains_key(&segment.text),
                        is_data_symbol(ctx, &segment.text),
                    ) {
                        hotspots.push(Hotspot {
                            line,
                            col,
                            target: segment.text.clone(),
                            kind,
                            code,
                        });
                    }
                }
                // A `0x…` literal in any view. Whether it names something is
                // decided here, against the section map.
                Tok::Hex => {
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
    /// 64-bit little-endian — spelled out at each use site now that `PtrFmt` has
    /// no `Default`, because defaulting *is* the bug this guards against.
    const LE64: PtrFmt = PtrFmt {
        width: 8,
        big_endian: false,
    };

    fn le64() -> Option<PtrFmt> {
        Some(LE64)
    }

    use super::{
        arch_ptr_fmt, call_stops, classify_name, covering_span, resolve_ptr_fmt, symbolize_dump,
        tab_stops, truncate_str_segment, valid_ident, HotKind, Hotspot, PtrFmt,
    };
    use std::collections::HashMap;

    #[test]
    fn classify_name_prefers_local_over_shadowing_global() {
        // A function name always wins.
        assert_eq!(classify_name(true, true, true), Some((HotKind::Func, true)));
        // A local shadows a same-named global data symbol (the bug fix): the
        // token stays a Local, so `n` renames the local and `g` targets it.
        assert_eq!(
            classify_name(false, true, true),
            Some((HotKind::Local, false))
        );
        // A plain global data symbol.
        assert_eq!(
            classify_name(false, false, true),
            Some((HotKind::Data, false))
        );
        // Nothing known → not a hotspot.
        assert_eq!(classify_name(false, false, false), None);
    }

    /// End-to-end: a demangled C++ callee must survive lexing in one piece and
    /// come out the other side as a `Func` hotspot, or `g`/`x`/`n` have nothing
    /// to act on and `w`/`W` step straight past the call.
    #[test]
    fn a_qualified_callee_becomes_a_func_hotspot() {
        let mut ctx = crate::ctx::Ctx::stub();
        // What `Ctx::build` stores: BN's mangled name and its display name.
        ctx.func_names.insert("_ZN3mtd3runEiPPc".into());
        ctx.func_names.insert("mtd::run".into());

        let lines = crate::syntax::tokenize_c("    return mtd::run(argc, argv);");
        let spans = super::build_spans(&lines, &ctx, &HashMap::new());

        let hit = spans
            .iter()
            .find(|s| s.kind == HotKind::Func)
            .expect("the qualified callee should be a Func hotspot");
        assert_eq!(hit.target, "mtd::run");
        assert!(hit.code);
        // Column must point at the `m`, not at the leading indent.
        assert_eq!(hit.col, "    return ".chars().count());
    }

    /// The data map builds its own segments instead of going through the lexer, so
    /// the `Tok::Hex` it tags a pointer with is the *only* thing making that pointer
    /// navigable now that the hotspot pass keys on the kind alone. Nothing else in
    /// the suite covers that coupling: retagging it `Tok::Num` leaves every other
    /// test green while `g`/`p` on a data-map pointer silently stops working.
    #[test]
    fn a_data_map_pointer_stays_an_addr_hotspot() {
        let mut ctx = crate::ctx::Ctx::stub();
        ctx.section_ranges = vec![(0x400000, 0x410000, ".text".into(), true)];
        let var = crate::bn::DataVar {
            addr: "0x1000".into(),
            name: "g_handler".into(),
            type_name: "void*".into(),
            width: 8,
            section: ".data".into(),
            value: None,
            ptr: Some("0x401230".into()),
            ptr_sym: None,
            ptr_str: None,
        };
        let out = crate::datamap::linear(".data", 0x1000, 0x1040, &[var], &[], 0x1000);
        let spans = super::build_spans(&out.lines, &ctx, &HashMap::new());

        let hit = spans
            .iter()
            .find(|s| s.target == "0x401230")
            .expect("the pointer target must be a hotspot");
        assert_eq!(hit.kind, HotKind::Addr);
        // 0x401230 is in .text, so `g` decompiles it rather than opening data.
        assert!(hit.code);
    }

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
    fn tab_includes_register_temp_locals_and_skips_self_name() {
        let spans = vec![
            spot("parse_frame", HotKind::Func, true, 0), // signature self-name
            spot("v0", HotKind::Local, false, 2),        // register temp — still a stop
            spot("v0_2", HotKind::Local, false, 2),
            spot("frame_len", HotKind::Local, false, 3),
            spot("strcpy", HotKind::Func, true, 4),
            spot("0x1000", HotKind::Addr, false, 5), // data address — actionable, a stop
            spot("0x4010", HotKind::Addr, true, 6),  // code address — a stop
            spot("boot", HotKind::Str, false, 7),
        ];
        // Only the self-name is skipped; temps + named locals + calls + *both*
        // code and data addresses stay (data addr `w`/`b` was the missing case).
        assert_eq!(tab_stops(&spans, "parse_frame"), vec![1, 2, 3, 4, 5, 6, 7]);
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
    fn call_stops_keeps_only_calls_and_code_addresses() {
        let spans = vec![
            spot("parse_frame", HotKind::Func, true, 0), // signature self-name — excluded
            spot("frame_len", HotKind::Local, false, 1), // local — excluded
            spot("strcpy", HotKind::Func, true, 2),      // a call — kept
            spot("0x1000", HotKind::Addr, false, 3),     // data address — excluded
            spot("0x4010", HotKind::Addr, true, 4),      // code address — kept
            spot("g_config", HotKind::Data, false, 5),   // data global — excluded
        ];
        assert_eq!(call_stops(&spans, "parse_frame"), vec![2, 4]);
        // No calls at all → empty (the caller reports "no calls", no ring fallback).
        let only_locals = vec![spot("v0", HotKind::Local, false, 0)];
        assert!(call_stops(&only_locals, "f").is_empty());
    }

    fn spot_at(target: &str, kind: HotKind, col: usize, line: usize) -> Hotspot {
        Hotspot {
            line,
            col,
            target: target.into(),
            kind,
            code: false,
        }
    }

    #[test]
    fn covering_span_selects_the_token_at_the_match_column() {
        // Line 3:  `x0_21` at col 8, `resolve_path` at col 16, `"hi"` at col 30.
        let spans = vec![
            spot_at("x0_21", HotKind::Local, 8, 3),
            spot_at("resolve_path", HotKind::Func, 16, 3),
            spot_at("hi", HotKind::Str, 30, 3), // stored without quotes; display spans 30..34
        ];
        // Match at the start of the call → the call, not the leftmost local.
        assert_eq!(covering_span(&spans, 3, 16), Some(1));
        // Match *inside* the call token still resolves to it.
        assert_eq!(covering_span(&spans, 3, 20), Some(1));
        // Match on the local resolves to the local.
        assert_eq!(covering_span(&spans, 3, 8), Some(0));
        // The string's quotes are part of its covered extent (30..34).
        assert_eq!(covering_span(&spans, 3, 33), Some(2));
        // A gap between tokens covers nothing (caller keeps its fallback).
        assert_eq!(covering_span(&spans, 3, 14), None);
        // Wrong line covers nothing.
        assert_eq!(covering_span(&spans, 2, 16), None);
    }

    #[test]
    fn data_symbol_addr_recovers_the_encoded_address() {
        use super::data_symbol_addr;
        // A synthetic `data_<hex>` name carries its own address.
        assert_eq!(data_symbol_addr("data_482049").as_deref(), Some("0x482049"));
        // Not a data_ name → nothing to recover.
        assert_eq!(data_symbol_addr("g_config"), None);
        assert_eq!(data_symbol_addr("data_"), None);
        assert_eq!(data_symbol_addr("data_xyz"), None);
        assert_eq!(data_symbol_addr("0x482049"), None);
    }

    #[test]
    fn truncates_long_string_segments_keeping_quotes() {
        assert_eq!(truncate_str_segment("\"short\"", 44), None);
        let long = format!("\"{}\"", "A".repeat(80));
        let out = truncate_str_segment(&long, 20).unwrap();
        assert!(out.starts_with('"'), "keeps opening quote: {out}");
        assert!(
            out.ends_with("…\""),
            "keeps ellipsis + closing quote: {out}"
        );
        assert!(out.chars().count() <= 21, "stays near the limit: {out}");
    }

    #[test]
    fn type_matches_ranks_prefix_then_substring() {
        let types: Vec<String> = ["char", "char*", "uchar", "unsigned char", "int", "wchar_t"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = super::type_matches(&types, "char", 10);
        // Exact match first, then the prefix `char*`, then substring matches.
        assert_eq!(out[0], "char");
        assert_eq!(out[1], "char*");
        assert!(out.contains(&"unsigned char".to_string()));
        assert!(out.contains(&"uchar".to_string()));
        // Empty query offers nothing.
        assert!(super::type_matches(&types, "  ", 10).is_empty());
        // Cap is honoured.
        assert!(super::type_matches(&types, "c", 2).len() <= 2);
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
        let output = symbolize_dump(dump, &reverse_symbols, Some(LE64));
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
        let output = symbolize_dump(dump, &reverse_symbols, Some(LE64));
        assert_eq!(output[0], dump);
    }

    #[test]
    fn symbolizes_pointers_at_the_target_width() {
        // A 32-bit handler table: two 4-byte little-endian entries, back to back.
        let mut reverse_symbols = HashMap::new();
        reverse_symbols.insert("0x11a30".to_string(), "handle_get".to_string());
        reverse_symbols.insert("0x11b04".to_string(), "handle_set".to_string());
        let dump = "0002b400: 30 1a 01 00 04 1b 01 00 00 00 00 00 00 00 00 00  0...............";
        // Read as 64-bit (the old hardcoded behaviour) neither entry resolves.
        assert_eq!(symbolize_dump(dump, &reverse_symbols, Some(LE64))[0], dump);
        // At the target's own pointer width both do, stepped at that stride.
        let output = symbolize_dump(dump, &reverse_symbols, arch_ptr_fmt("armv7"));
        assert!(
            output[0].contains("+0x0→handle_get") && output[0].contains("+0x4→handle_set"),
            "got: {}",
            output[0]
        );
    }

    #[test]
    fn symbolizes_a_big_endian_pointer() {
        let mut reverse_symbols = HashMap::new();
        reverse_symbols.insert("0x11a30".to_string(), "handle_get".to_string());
        let dump = "0002b400: 00 01 1a 30 00 00 00 00 00 00 00 00 00 00 00 00  ...0............";
        let output = symbolize_dump(dump, &reverse_symbols, arch_ptr_fmt("mips32"));
        assert!(output[0].contains("+0x0→handle_get"), "got: {}", output[0]);
        // The same bytes on the little-endian sibling architecture are not a
        // pointer, and must not be annotated.
        assert_eq!(
            symbolize_dump(dump, &reverse_symbols, arch_ptr_fmt("mipsel32"))[0],
            dump
        );
    }

    #[test]
    fn the_architecture_name_table_never_guesses() {
        let le = |width| {
            Some(PtrFmt {
                width,
                big_endian: false,
            })
        };
        let be = |width| {
            Some(PtrFmt {
                width,
                big_endian: true,
            })
        };
        assert_eq!(arch_ptr_fmt("x86_64"), le(8));
        assert_eq!(arch_ptr_fmt("aarch64"), le(8));
        assert_eq!(arch_ptr_fmt("x86"), le(4));
        assert_eq!(arch_ptr_fmt("armv7"), le(4));
        assert_eq!(arch_ptr_fmt("thumb2"), le(4));
        assert_eq!(arch_ptr_fmt("armv7eb"), be(4));
        assert_eq!(arch_ptr_fmt("mips32"), be(4));
        assert_eq!(arch_ptr_fmt("mipsel32"), le(4));
        assert_eq!(arch_ptr_fmt("ppc"), be(4));
        assert_eq!(arch_ptr_fmt("ppc_le"), le(4));
        assert_eq!(arch_ptr_fmt("ppc64"), be(8));
        assert_eq!(arch_ptr_fmt("ppc64_le"), le(8));
        assert_eq!(arch_ptr_fmt("rv32gc"), le(4));
        assert_eq!(arch_ptr_fmt("x86_16"), le(2));
        // Case/whitespace tolerated.
        assert_eq!(arch_ptr_fmt(" ARMv7 "), le(4));
        // A valid BN/custom architecture the table never enumerated (SPARC is the
        // immediate real example) must not decode as 64-bit little-endian. A
        // misdecoded word can collide with a real reverse-symbol address and print
        // a confident wrong name, which is worse than no annotation.
        assert_eq!(arch_ptr_fmt("sparcv9"), None);
        assert_eq!(arch_ptr_fmt("some-future-arch"), None);
        assert_eq!(arch_ptr_fmt(""), None);
    }

    #[test]
    fn reported_target_metadata_outranks_the_architecture_name() {
        // What BN itself says (`bv.address_size` / `bv.endianness`) is authoritative
        // for *any* architecture, including one the name table has wrong or has
        // never heard of.
        assert_eq!(
            resolve_ptr_fmt(Some(4), Some(true), "sparcv9"),
            Some(PtrFmt {
                width: 4,
                big_endian: true
            })
        );
        assert_eq!(
            resolve_ptr_fmt(Some(8), Some(false), "mips32"),
            Some(LE64),
            "reported metadata wins over the table's big-endian 4-byte guess"
        );
        // Half-reported (one field missing) is not enough to build a format from,
        // so it falls back to the table rather than pairing a real width with an
        // assumed byte order.
        assert_eq!(
            resolve_ptr_fmt(Some(4), None, "mips32"),
            arch_ptr_fmt("mips32")
        );
        assert_eq!(resolve_ptr_fmt(None, Some(true), "sparcv9"), None);
        // An out-of-range width is a payload surprise, not a target fact.
        assert_eq!(resolve_ptr_fmt(Some(0), Some(false), "sparcv9"), None);
        assert_eq!(resolve_ptr_fmt(Some(99), Some(false), "x86_64"), le64());
    }

    #[test]
    fn an_unknown_pointer_format_annotates_nothing() {
        // The dump comes back verbatim — no guessed-width reads at all.
        let mut reverse_symbols = HashMap::new();
        reverse_symbols.insert("0x402760".to_string(), "handle_inbound_c2_msg".to_string());
        let dump = "00415258: 00 00 00 00 00 00 00 00 60 27 40 00 00 00 00 00  ........";
        assert_eq!(symbolize_dump(dump, &reverse_symbols, None), vec![dump]);
    }
}
