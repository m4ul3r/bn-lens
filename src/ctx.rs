//! Shared, loaded-once context: the resolved bn instance + target, its
//! functions and data symbols, the address maps, and the launching pane.

use crate::bn::{self, AnalysisState, Bn, Func};
use crate::herdr;
use std::collections::{HashMap, HashSet};

pub struct Ctx {
    pub bn: Bn,
    pub herdr: String,
    pub agent_pane: String,
    /// Session id of the agent that spawned the lens (from BN_LENS_AGENT_SESSION);
    /// empty if unknown. Used to verify the ask target is still that same agent.
    pub agent_session: String,
    pub instance_label: String,
    pub target: String, // the `-t` selector
    pub arch: String,
    /// Completeness is security provenance: only `Full` permits authoritative
    /// absence wording in usage/list views.
    pub analysis_state: AnalysisState,
    pub funcs: Vec<Func>,
    pub func_names: HashSet<String>,
    pub import_names: HashSet<String>,
    pub data_names: HashSet<String>,
    pub addr_by_name: HashMap<String, String>,
    pub name_by_addr: HashMap<String, String>,
    /// Stable/display aliases -> the human-facing demangled name.
    pub display_by_name: HashMap<String, String>,
    /// Raw `bn sections` lines (for the section popup).
    pub sections_text: Vec<String>,
    /// Parsed section ranges: (start, end, name, executable).
    pub section_ranges: Vec<(u64, u64, String, bool)>,
    /// Lazily-built string-literal map: content -> address (preferring the real
    /// `.rodata` copy over `.dynstr`/`.symtab` duplicates).
    strings_map: std::cell::OnceCell<HashMap<String, String>>,
}

/// Rank a section for string resolution: real read-only data beats other data
/// beats the symbol-table sections (`.dynstr`/`.dynsym`/`.strtab`) that
/// duplicate strings. The rodata check must run first — GCC's merged-string
/// sections (`.rodata.str1.1`) contain "str" and would otherwise sink into the
/// symbol-table bucket; PE's `.rdata` is the same idea under a different name.
fn sec_rank(name: &str) -> u8 {
    if name.starts_with(".rodata") || name == ".rdata" {
        0
    } else if name.contains("dyn") || name.contains("sym") || name.contains("str") {
        3
    } else {
        1
    }
}

/// Parse `0x…` (or bare hex) to a value.
pub fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().strip_prefix("0x").unwrap_or(s.trim()), 16).ok()
}

/// Parse `bn sections` lines into (start, end, name, executable) ranges.
fn parse_section_ranges(lines: &[String]) -> Vec<(u64, u64, String, bool)> {
    let mut out = Vec::new();
    for line in lines {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let Some(range) = cols.first() else { continue };
        let Some((s, e)) = range.split_once('-') else {
            continue;
        };
        let (Some(s), Some(e)) = (parse_hex(s), parse_hex(e)) else {
            continue;
        };
        let name = cols.last().copied().unwrap_or("").to_string();
        // perms column (e.g. "r-x") is the 3rd token when present
        let exec = cols.get(2).is_some_and(|p| p.contains('x'));
        out.push((s, e, name, exec));
    }
    out
}

impl Ctx {
    pub fn analysis_complete(&self) -> bool {
        self.analysis_state.is_complete()
    }

    pub fn analysis_warning(&self) -> Option<String> {
        (!self.analysis_complete()).then(|| {
            format!(
                "⚠ {} analysis — results and absences are incomplete",
                self.analysis_state.label()
            )
        })
    }

    pub fn display_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.display_by_name
            .get(name)
            .map(String::as_str)
            .unwrap_or(name)
    }

    /// The section containing `addr`, if any.
    pub fn section_of(&self, addr: u64) -> Option<&(u64, u64, String, bool)> {
        self.section_ranges
            .iter()
            .find(|(s, e, _, _)| addr >= *s && addr < *e)
    }

    /// Triage priority for a string at `addr` (lower = shown first): real
    /// `.rodata` literals lead, then other data, then the symbol-table sections
    /// (`.dynstr`/`.dynsym`/`.strtab`) and header noise that otherwise floats to
    /// the top under a pure address sort. `4` for an address in no section.
    pub fn string_rank(&self, addr: u64) -> u8 {
        self.section_of(addr)
            .map_or(4, |(_, _, name, _)| sec_rank(name))
    }

    /// String-literal map (content -> address), built once on first use.
    pub fn strings(&self) -> &HashMap<String, String> {
        self.strings_map.get_or_init(|| {
            let mut best: HashMap<String, (u8, String)> = HashMap::new();
            for (content, addr) in self.bn.strings() {
                let rank = parse_hex(&addr)
                    .and_then(|v| self.section_of(v))
                    .map_or(2, |(_, _, n, _)| sec_rank(n));
                match best.get(&content) {
                    Some((r, _)) if *r <= rank => {}
                    _ => {
                        best.insert(content, (rank, addr));
                    }
                }
            }
            best.into_iter().map(|(k, (_, a))| (k, a)).collect()
        })
    }
}

impl Ctx {
    /// Auto-resolve the instance (marker → single → newest live, self-healing
    /// past an empty instance), then build.
    pub fn load() -> Result<Ctx, String> {
        let bn_bin = bn::resolve_bin("bn", "BN_LENS_BN_PATH", &["~/.local/bin/bn"]);
        let herdr_bin = herdr::bin();
        let cwd = std::env::var("BN_LENS_CWD").unwrap_or_default();
        let agent_pane = std::env::var("BN_LENS_PANE").unwrap_or_default();

        let instance = bn::resolve_instance(&bn_bin, &cwd);
        let mut failures = Vec::new();
        if let Some(inst) = &instance {
            match Self::build(&bn_bin, &herdr_bin, &agent_pane, Some(inst.clone()), None) {
                Ok(ctx) => return Ok(ctx),
                Err(error) => failures.push(format!("{inst}: {error}")),
            }
        }
        let mut live = Bn::session_list(&bn_bin);
        live.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        for alt in live.into_iter().filter(|i| !i.binaries.is_empty()) {
            if Some(&alt.instance_id) == instance.as_ref() {
                continue;
            }
            match Self::build(
                &bn_bin,
                &herdr_bin,
                &agent_pane,
                Some(alt.instance_id.clone()),
                None,
            ) {
                Ok(ctx) => return Ok(ctx),
                Err(error) => failures.push(format!("{}: {error}", alt.instance_id)),
            }
        }
        let detail = failures
            .into_iter()
            .take(3)
            .collect::<Vec<_>>()
            .join("\n  ");
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!("\n  {detail}")
        };
        Err(format!(
            "no usable target in any live bn instance.{detail}\n  \
             Open a bn session for this pane's binary, or set BN_LENS_INSTANCE."
        ))
    }

    /// Build the context for a specific instance + target (target = None picks
    /// the active target). Errs if the target has no functions (lets `load`
    /// self-heal, and the switcher reject a bad pick).
    pub fn build(
        bn_bin: &str,
        herdr_bin: &str,
        agent_pane: &str,
        instance: Option<String>,
        target: Option<String>,
    ) -> Result<Ctx, String> {
        let mut bn = Bn::new(bn_bin.to_string(), instance.clone(), target.clone());
        let target_sel = match target {
            Some(t) => {
                bn.target = Some(t.clone());
                t
            }
            None => {
                let tl = bn.target_list_checked()?;
                let sel = tl
                    .iter()
                    .find(|t| t.active)
                    .or_else(|| tl.first())
                    .map(|t| t.selector.clone());
                bn.target = sel.clone();
                sel.unwrap_or_default()
            }
        };

        let target_info = bn.target_info()?;
        let funcs = bn.functions_checked()?;
        if funcs.is_empty() {
            return Err(format!("{instance:?} / {target_sel} has no functions"));
        }

        let (mut addr_by_name, data_names) = bn.symbols_checked()?;
        let mut func_names = HashSet::new();
        let mut name_by_addr = HashMap::new();
        let mut display_by_name = HashMap::new();
        for f in &funcs {
            addr_by_name.entry(f.name.clone()).or_insert(f.addr.clone());
            addr_by_name
                .entry(f.display_name.clone())
                .or_insert(f.addr.clone());
            func_names.insert(f.name.clone());
            func_names.insert(f.display_name.clone());
            display_by_name.insert(f.name.clone(), f.display_name.clone());
            display_by_name.insert(f.display_name.clone(), f.display_name.clone());
            // Canonical/raw identity wins for mutations and backend commands.
            name_by_addr
                .entry(f.addr.clone())
                .or_insert_with(|| f.name.clone());
        }
        for (name, addr) in &addr_by_name {
            name_by_addr
                .entry(addr.clone())
                .or_insert_with(|| name.clone());
        }
        let import_names = bn.imports_checked()?;
        let sections_text = bn.sections_checked()?;
        let section_ranges = parse_section_ranges(&sections_text);

        Ok(Ctx {
            instance_label: instance.unwrap_or_else(|| "(default)".into()),
            target: target_sel,
            arch: target_info.arch,
            analysis_state: target_info.analysis_state,
            bn,
            herdr: herdr_bin.to_string(),
            agent_pane: agent_pane.to_string(),
            agent_session: std::env::var("BN_LENS_AGENT_SESSION").unwrap_or_default(),
            funcs,
            func_names,
            import_names,
            data_names,
            addr_by_name,
            name_by_addr,
            display_by_name,
            sections_text,
            section_ranges,
            strings_map: std::cell::OnceCell::new(),
        })
    }
}

/// A human-recognizable key for a target selector: its binary basename, with
/// any path, `.bndb` suffix, and trailing `.<hash>` segment stripped
/// (`…/mosquitto.65d2…ca.bndb` → `mosquitto`). `None` for an empty selector.
fn target_key(target: &str) -> Option<String> {
    let t = target.trim();
    if t.is_empty() {
        return None;
    }
    let base = t.rsplit(['/', '\\']).next().unwrap_or(t);
    let base = base.strip_suffix(".bndb").unwrap_or(base);
    // Drop a trailing `.<hex>` (bn's `name.<contenthash>` bndb convention).
    let base = match base.rsplit_once('.') {
        Some((head, tail)) if tail.len() >= 6 && tail.chars().all(|c| c.is_ascii_hexdigit()) => {
            head
        }
        _ => base,
    };
    (!base.is_empty()).then(|| base.to_lowercase())
}

impl Ctx {
    /// Agent-referenced tokens scoped to transcript regions whose most recent
    /// explicit `-t/--target` selector names this target. This is intentionally
    /// fail-closed: a target name appearing once somewhere in mixed scrollback
    /// no longer blesses unrelated addresses from every other target.
    pub fn recent_agent_tokens(&self, transcript: &str) -> Vec<String> {
        scan_recent_for_target(transcript, &self.target)
    }
}

fn explicit_target(line: &str) -> Option<&str> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if let Some(target) = tokens.iter().rev().find_map(|token| {
        token
            .strip_prefix("--target=")
            .or_else(|| token.strip_prefix("-t="))
    }) {
        return Some(target.trim_matches(|ch: char| {
            matches!(ch, '`' | '\'' | '"' | ',' | ';' | ')' | ']' | '}')
        }));
    }
    tokens.windows(2).rev().find_map(|pair| {
        matches!(pair[0], "-t" | "--target").then(|| {
            pair[1].trim_matches(|ch: char| {
                matches!(ch, '`' | '\'' | '"' | ',' | ';' | ')' | ']' | '}')
            })
        })
    })
}

/// Keep only lines within explicit target-scoped transcript regions, then run
/// the normal recency scanner over that subset.
pub fn scan_recent_for_target(transcript: &str, target: &str) -> Vec<String> {
    let Some(want) = target_key(target) else {
        return scan_recent(transcript);
    };
    let mut active = false;
    let mut scoped = String::new();
    for line in transcript.lines() {
        if let Some(selector) = explicit_target(line) {
            active = target_key(selector).as_deref() == Some(want.as_str());
        }
        if active {
            scoped.push_str(line);
            scoped.push('\n');
        }
    }
    scan_recent(&scoped)
}

/// Identifier / address tokens in `text`, ordered most-recently-mentioned first
/// (by last occurrence). Feeds the picker's "recently viewed by agent" group.
/// Identifiers are ≥3 chars; hex addresses are `0x` + ≥3 hex digits.
pub fn scan_recent(text: &str) -> Vec<String> {
    let ch: Vec<char> = text.chars().collect();
    let n = ch.len();
    let mut last: HashMap<String, usize> = HashMap::new();
    let mut i = 0;
    while i < n {
        let c = ch[i];
        if c == '0' && i + 1 < n && ch[i + 1] == 'x' {
            let start = i;
            i += 2;
            while i < n && ch[i].is_ascii_hexdigit() {
                i += 1;
            }
            if i - start >= 5 {
                last.insert(ch[start..i].iter().collect(), start);
            }
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < n && (ch[i].is_ascii_alphanumeric() || ch[i] == '_') {
                i += 1;
            }
            if i - start >= 3 {
                last.insert(ch[start..i].iter().collect(), start);
            }
        } else {
            i += 1;
        }
    }
    let mut v: Vec<(String, usize)> = last.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1)); // most recent (largest position) first
    v.into_iter().map(|(t, _)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::{scan_recent_for_target, sec_rank, target_key};

    #[test]
    fn target_key_strips_path_hash_and_extension() {
        assert_eq!(
            target_key("/home/u/.cache/bn/bndb/mosquitto.65d26f3541c254ca.bndb").as_deref(),
            Some("mosquitto")
        );
        assert_eq!(
            target_key("sample_daemon").as_deref(),
            Some("sample_daemon")
        );
        assert_eq!(
            target_key("radio_service.bndb").as_deref(),
            Some("radio_service")
        );
        assert_eq!(target_key(""), None);
    }

    #[test]
    fn target_key_keeps_a_non_hash_dotted_name() {
        // A trailing segment that isn't a hex hash stays put.
        assert_eq!(target_key("libssl.so").as_deref(), Some("libssl.so"));
    }

    #[test]
    fn rodata_ranks_ahead_of_symbol_table_sections() {
        assert!(sec_rank(".rodata") < sec_rank(".data"));
        assert!(sec_rank(".data") < sec_rank(".dynstr"));
        assert!(sec_rank(".text") < sec_rank(".strtab"));
        // Merged-string rodata and PE .rdata rank as real rodata, not noise,
        // even though ".rodata.str1.1" contains "str".
        assert_eq!(sec_rank(".rodata.str1.1"), sec_rank(".rodata"));
        assert_eq!(sec_rank(".rdata"), 0);
        assert!(sec_rank(".rodata.str1.8") < sec_rank(".dynstr"));
    }

    #[test]
    fn recent_scan_is_scoped_by_explicit_target_regions() {
        let transcript = "\
bn -i demo -t radio_service decompile parse_tlv
0x401111 memcpy parse_tlv
bn -i demo -t media_plugin.so decompile copy_body
0x402222 __memcpy_chk copy_body
[bn lens] --target=media_plugin.so · sub_4013a0 @ 0x402223
[bn lens] -i demo -t radio_service · tlv_next @ 0x403333
0x403334 near_tlv_parse
";
        let radio = scan_recent_for_target(transcript, "radio_service");
        assert!(radio.contains(&"0x401111".to_string()));
        assert!(radio.contains(&"0x403334".to_string()));
        assert!(!radio.contains(&"0x402222".to_string()));

        let plugin = scan_recent_for_target(transcript, "media_plugin.so");
        assert!(plugin.contains(&"0x402222".to_string()));
        assert!(plugin.contains(&"0x402223".to_string()));
        assert!(!plugin.contains(&"0x403334".to_string()));
    }

    #[test]
    fn recent_scan_keeps_long_followup_discussion_in_matching_regions() {
        // Match the 400-line recent-unwrapped capture shape: unscoped history,
        // a long discussion after one selector, another target, then a return
        // to the original target. Region persistence is deliberate—it keeps
        // the agent's prose/results after a command instead of reducing recent
        // references to the selector line itself.
        let transcript = format!(
            "{}[bn lens] -i demo -t radio_service · parse_frame @ 0x401000\n{}\
             inspected caller 0x401abc parse_frame_tail\n\
             [bn lens] -i demo -t media_plugin.so · decode_item @ 0x502000\n{}\
             unrelated caller 0x502def decode_item_tail\n\
             [bn lens] -i demo -t radio_service · dispatch_frame @ 0x403000\n{}\
             final caller 0x403fed dispatch_frame_tail\n",
            "old unscoped output\n".repeat(80),
            "continued radio analysis\n".repeat(100),
            "continued media analysis\n".repeat(100),
            "continued radio followup\n".repeat(100),
        );

        let radio = scan_recent_for_target(&transcript, "radio_service");
        assert!(radio.contains(&"0x401abc".to_string()));
        assert!(radio.contains(&"0x403fed".to_string()));
        assert!(!radio.contains(&"0x502def".to_string()));

        let plugin = scan_recent_for_target(&transcript, "media_plugin.so");
        assert!(plugin.contains(&"0x502def".to_string()));
        assert!(!plugin.contains(&"0x403fed".to_string()));
    }

    #[test]
    fn recent_scan_fails_closed_without_a_target_marker() {
        let tokens = scan_recent_for_target("radio_service maybe 0x401111", "radio_service");
        assert!(tokens.is_empty());
    }
}
