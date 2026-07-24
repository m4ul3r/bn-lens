//! "Where is this used?" report — shared by the Strings and Imports views' `p`
//! peek. Given an address, parse `bn xrefs`, decompile each referencing function
//! once (`--addresses`), and render exact disassembly plus an explicitly
//! approximate pseudo-C mapping at each callsite, plus any data-ref lines.

use crate::bn::Bn;
use crate::ctx::Ctx;
use crate::decomp::{addr_lines, lines_at};
use std::collections::HashMap;

/// Caps that bound a peek's latency: one decompile per referencing function
/// (~hundreds of ms each) and a total site count. The full set is one `x` away.
const MAX_SITES: usize = 12;
const MAX_FUNCS: usize = 6;
/// Address-prefix mappings can land on a declaration immediately beside the
/// actual call. Hint rescue is useful there, but searching the whole function
/// lets short/common string content select an unrelated statement.
const MAX_HINT_RESCUE_DISTANCE: u64 = 0x40;
const MIN_HINT_RESCUE_CHARS: usize = 4;

/// The slice of [`Ctx`] a usage report actually reads, snapshotted so the
/// report can run on a worker thread (`Ctx` itself is not `Send` — it holds a
/// lazily-built `OnceCell`). `Bn` clones share the socket client and failure
/// state by `Arc`, so backend errors still surface in the shared status bar.
pub struct UsageSource {
    bn: Bn,
    analysis_warning: Option<String>,
    analysis_complete: bool,
    display_by_name: HashMap<String, String>,
}

impl UsageSource {
    pub fn from_ctx(ctx: &Ctx) -> Self {
        UsageSource {
            bn: ctx.bn.clone(),
            analysis_warning: ctx.analysis_warning(),
            analysis_complete: ctx.analysis_complete(),
            display_by_name: ctx.display_by_name.clone(),
        }
    }

    fn display_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.display_by_name
            .get(name)
            .map(String::as_str)
            .unwrap_or(name)
    }
}

/// The popup body while the report worker runs. Rebuilt each tick by the app
/// so the elapsed counter moves; Esc closes the popup and the app discards the
/// in-flight result on delivery.
pub fn loading_lines(elapsed_secs: f32) -> Vec<String> {
    vec![format!("⟳ loading usage…  {elapsed_secs:.1}s   · Esc to close")]
}

/// Build the usage-report lines for `addr` (the popup body). Empty references
/// yield a single analysis-qualified explanatory line. Runs on a worker thread
/// (the `p` peek), so it takes the [`UsageSource`] snapshot rather than `&Ctx`.
pub fn report(src: &UsageSource, addr: &str, hint: &str) -> Vec<String> {
    let raw_xrefs = match src.bn.xrefs_checked(addr) {
        Ok(xrefs) => xrefs,
        Err(error) => return vec![format!("✗ {error}")],
    };
    let (code, data) = parse_xrefs(&raw_xrefs);
    let mut lines: Vec<String> = Vec::new();
    if let Some(warning) = &src.analysis_warning {
        lines.push(warning.clone());
        lines.push(String::new());
    }
    if code.is_empty() && data.is_empty() {
        lines.push(if src.analysis_complete {
            "no code or data references".into()
        } else {
            "no references observed in incomplete analysis".into()
        });
    }
    if !code.is_empty() {
        lines.push("code:".into());
        let mut sites_left = MAX_SITES;
        let mut funcs_left = MAX_FUNCS;
        'outer: for (func, sites) in &code {
            if funcs_left == 0 {
                lines.push("  … more functions — x for the full list".into());
                break;
            }
            funcs_left -= 1;
            lines.push(format!("  {}", src.display_name(func)));
            let dec = addr_lines(&src.bn.decompile_addr(func));
            for site in sites {
                if sites_left == 0 {
                    lines.push("    … more sites — x for the full list".into());
                    break 'outer;
                }
                sites_left -= 1;
                // The exact machine instruction is the authority. BN's
                // address-prefixed decompile can assign a call address to a
                // declaration/neighboring statement, so pseudo-C is explicitly
                // approximate and never replaces the instruction evidence.
                lines.push(format!("    {site}  asm: {}", disasm_line(&src.bn, site)));
                let matched = crate::ctx::parse_hex(site)
                    .map(|cs| best_decompile_lines(&dec, cs, hint))
                    .unwrap_or_default();
                for (i, text) in matched.iter().take(2).enumerate() {
                    if i == 0 {
                        lines.push(format!("               C≈ {text}"));
                    } else {
                        lines.push(format!("                  {text}"));
                    }
                }
            }
        }
    }
    if !data.is_empty() {
        if !code.is_empty() {
            lines.push(String::new());
        }
        lines.push("data:".into());
        lines.extend(data.into_iter().map(|d| format!("  {d}")));
    }
    lines
}

/// Prefer a *nearby* pseudo-C line that actually names the referenced import /
/// export / string. If none does, retain the ordinary address mapping, but the
/// caller labels it `C≈` and has already rendered exact disassembly first.
fn best_decompile_lines(dec: &[(u64, String)], site: u64, hint: &str) -> Vec<String> {
    let mapped: Vec<String> = lines_at(dec, site)
        .into_iter()
        .map(str::to_string)
        .collect();
    let hint = hint.trim_matches('"').trim().to_ascii_lowercase();
    if hint.is_empty()
        || mapped
            .iter()
            .any(|line| line.to_ascii_lowercase().contains(&hint))
    {
        return mapped;
    }

    // Tiny literals/identifiers are too collision-prone to override even an
    // approximate address mapping. Exact disassembly above remains available.
    if hint.chars().count() < MIN_HINT_RESCUE_CHARS {
        return mapped;
    }

    let closest = dec
        .iter()
        .filter(|(addr, text)| {
            addr.abs_diff(site) <= MAX_HINT_RESCUE_DISTANCE
                && text.to_ascii_lowercase().contains(&hint)
        })
        .min_by_key(|(addr, _)| addr.abs_diff(site))
        .map(|(addr, _)| *addr);
    match closest {
        Some(addr) => dec
            .iter()
            .filter(|(candidate, _)| *candidate == addr)
            .map(|(_, text)| text.clone())
            .collect(),
        None => mapped,
    }
}

/// Parse `bn xrefs` text into ([(function, [callsite addrs])], [data-ref lines]).
/// Code lines look like `0x<fa>  <name>  (N sites: 0x.., 0x..)`; data-ref lines
/// are kept verbatim (their exact shape varies, and we only display them).
fn parse_xrefs(text: &str) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let mut code: Vec<(String, Vec<String>)> = Vec::new();
    let mut data: Vec<String> = Vec::new();
    let mut section = 0u8; // 1 = code refs, 2 = data refs
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("code refs") {
            section = 1;
            continue;
        }
        if t.starts_with("data refs") {
            section = 2;
            continue;
        }
        if t.is_empty() || t == "- none" || t.starts_with("xrefs to") {
            continue;
        }
        match section {
            1 => {
                let mut toks = t.split_whitespace();
                let _func_addr = toks.next();
                let name = toks.next().unwrap_or("").to_string();
                let sites: Vec<String> = toks
                    .filter_map(|w| {
                        let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                        w.starts_with("0x").then(|| w.to_string())
                    })
                    .collect();
                if !name.is_empty() {
                    code.push((name, sites));
                }
            }
            2 => data.push(t.to_string()),
            _ => {}
        }
    }
    (code, data)
}

/// The instruction line at `addr` (first non-comment line of a 1-instruction
/// linear disasm), trimmed; falls back to the bare address on any miss.
fn disasm_line(bn: &Bn, addr: &str) -> String {
    bn.disasm_linear(addr, 1)
        .lines()
        .map(str::trim_end)
        .find(|l| !l.trim_start().starts_with("//") && !l.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| addr.to_string())
}

#[cfg(test)]
mod tests {
    use super::{best_decompile_lines, loading_lines, parse_xrefs};

    #[test]
    fn loading_lines_show_elapsed_and_dismiss_hint() {
        let lines = loading_lines(0.0);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("loading usage"));
        assert!(lines[0].contains("0.0s"));
        assert!(lines[0].contains("Esc"));
        // The counter must actually move as the app re-renders it each tick.
        assert!(loading_lines(2.34)[0].contains("2.3s"));
    }

    #[test]
    fn parses_code_callsites_and_data_refs() {
        let text = "\
xrefs to 0x4016d0 (5 code, 1 data)

code refs: 5 sites across 2 functions
  0x40d040  add_resource_record  (2 sites: 0x40d214, 0x40d620)
  0x410240  expand_buf  (3 sites: 0x41028c, 0x4102a0, 0x4102b4)

data refs:
  0x4152a0  .data  ptr_table
";
        let (code, data) = parse_xrefs(text);
        assert_eq!(code.len(), 2);
        assert_eq!(code[0].0, "add_resource_record");
        assert_eq!(code[0].1, vec!["0x40d214", "0x40d620"]);
        assert_eq!(code[1].1.len(), 3);
        assert_eq!(code[1].1[2], "0x4102b4");
        assert_eq!(data, vec!["0x4152a0  .data  ptr_table".to_string()]);
    }

    #[test]
    fn handles_no_references() {
        let text =
            "xrefs to 0x400238 (0 code, 0 data)\n\ncode refs:\n- none\n\ndata refs:\n- none\n";
        let (code, data) = parse_xrefs(text);
        assert!(code.is_empty());
        assert!(data.is_empty());
    }

    #[test]
    fn hint_beats_a_misattributed_exact_decompile_line() {
        let dec = vec![
            (0x401000, "int128_t v0;".to_string()),
            (0x401004, "memcpy(dst, src, len);".to_string()),
            (0x402000, "memcpy(unrelated, far, away);".to_string()),
        ];
        assert_eq!(
            best_decompile_lines(&dec, 0x401000, "memcpy"),
            vec!["memcpy(dst, src, len);"]
        );
        assert_eq!(
            best_decompile_lines(&dec, 0x401000, "unseen"),
            vec!["int128_t v0;"]
        );
    }

    #[test]
    fn hint_rescue_rejects_distant_or_short_common_matches() {
        let dec = vec![
            (0x401000, "int128_t v0;".to_string()),
            (0x401004, "log_error(\"ok\");".to_string()),
            (0x402000, "memcpy(unrelated, far, away);".to_string()),
        ];
        assert_eq!(
            best_decompile_lines(&dec, 0x401000, "memcpy"),
            vec!["int128_t v0;"]
        );
        assert_eq!(
            best_decompile_lines(&dec, 0x401000, "ok"),
            vec!["int128_t v0;"]
        );
    }
}
