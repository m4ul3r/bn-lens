//! Shared, loaded-once context: the resolved bn instance + target, its
//! functions and data symbols, the address maps, and the launching pane.

use crate::bn::{self, Bn, Func};
use crate::herdr;
use std::collections::{HashMap, HashSet};

pub struct Ctx {
    pub bn: Bn,
    pub herdr: String,
    pub agent_pane: String,
    /// Session id of the agent that spawned the lens (from BN_LENS_AGENT_SESSION);
    /// empty if unknown. Used to verify the `?` target is still that same agent.
    pub agent_session: String,
    pub instance_label: String,
    pub target: String, // the `-t` selector
    pub arch: String,
    pub funcs: Vec<Func>,
    pub func_names: HashSet<String>,
    pub import_names: HashSet<String>,
    pub data_names: HashSet<String>,
    pub addr_by_name: HashMap<String, String>,
    pub name_by_addr: HashMap<String, String>,
    /// Raw `bn sections` lines (for the section popup).
    pub sections_text: Vec<String>,
    /// Parsed section ranges: (start, end, name, executable).
    pub section_ranges: Vec<(u64, u64, String, bool)>,
    /// Lazily-built string-literal map: content -> address (preferring the real
    /// `.rodata` copy over `.dynstr`/`.symtab` duplicates).
    strings_map: std::cell::OnceCell<HashMap<String, String>>,
}

/// Rank a section for string resolution: real rodata beats other data beats the
/// symbol-table sections (`.dynstr`/`.dynsym`/`.strtab`) that duplicate strings.
fn sec_rank(name: &str) -> u8 {
    if name == ".rodata" {
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
        let Some((s, e)) = range.split_once('-') else { continue };
        let (Some(s), Some(e)) = (parse_hex(s), parse_hex(e)) else { continue };
        let name = cols.last().copied().unwrap_or("").to_string();
        // perms column (e.g. "r-x") is the 3rd token when present
        let exec = cols.get(2).is_some_and(|p| p.contains('x'));
        out.push((s, e, name, exec));
    }
    out
}

impl Ctx {
    /// The section containing `addr`, if any.
    pub fn section_of(&self, addr: u64) -> Option<&(u64, u64, String, bool)> {
        self.section_ranges.iter().find(|(s, e, _, _)| addr >= *s && addr < *e)
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
        if let Some(inst) = &instance {
            if let Ok(c) = Self::build(&bn_bin, &herdr_bin, &agent_pane, Some(inst.clone()), None) {
                return Ok(c);
            }
        }
        let mut live = Bn::session_list(&bn_bin);
        live.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        for alt in live.into_iter().filter(|i| !i.binaries.is_empty()) {
            if Some(&alt.instance_id) == instance.as_ref() {
                continue;
            }
            if let Ok(c) = Self::build(&bn_bin, &herdr_bin, &agent_pane, Some(alt.instance_id), None) {
                return Ok(c);
            }
        }
        Err("no functions in any live bn instance.\n  \
             Open a bn session for this pane's binary, or set BN_LENS_INSTANCE."
            .into())
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
                let tl = bn.target_list();
                let sel = tl
                    .iter()
                    .find(|t| t.active)
                    .or_else(|| tl.first())
                    .map(|t| t.selector.clone());
                bn.target = sel.clone();
                sel.unwrap_or_default()
            }
        };

        let funcs = bn.functions();
        if funcs.is_empty() {
            return Err(format!("{instance:?} / {target_sel} has no functions"));
        }

        let (mut addr_by_name, data_names) = bn.symbols();
        for f in &funcs {
            addr_by_name.entry(f.name.clone()).or_insert(f.addr.clone());
        }
        let func_names: HashSet<String> = funcs.iter().map(|f| f.name.clone()).collect();
        let name_by_addr: HashMap<String, String> =
            addr_by_name.iter().map(|(n, a)| (a.clone(), n.clone())).collect();
        let import_names = bn.imports();
        let arch = bn.arch();
        let sections_text = bn.sections();
        let section_ranges = parse_section_ranges(&sections_text);

        Ok(Ctx {
            instance_label: instance.unwrap_or_else(|| "(default)".into()),
            target: target_sel,
            arch,
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
            sections_text,
            section_ranges,
            strings_map: std::cell::OnceCell::new(),
        })
    }
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
