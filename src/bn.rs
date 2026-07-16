//! Thin wrappers over the `bn` CLI plus instance resolution.
//!
//! Big reads (decompile) are captured via `--out <file>` to sidestep bn's
//! stdout "spill envelope" for large output; small reads use stdout directly.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

/// Resolve a binary: env override, then PATH, then known fallbacks. Plugin
/// panes launch with a minimal PATH that omits ~/.local/bin, so we can't rely
/// on a bare name.
pub fn resolve_bin(name: &str, env_key: &str, fallbacks: &[&str]) -> String {
    if let Ok(p) = std::env::var(env_key) {
        if !p.is_empty() && std::path::Path::new(&p).exists() {
            return p;
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = PathBuf::from(dir).join(name);
            if cand.exists() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    for fb in fallbacks {
        let expanded = expand_home(fb);
        if std::path::Path::new(&expanded).exists() {
            return expanded;
        }
    }
    name.to_string()
}

fn expand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

/// A bn CLI handle bound to a resolved binary path + instance + target.
#[derive(Clone)]
pub struct Bn {
    pub bin: String,
    pub instance: Option<String>,
    pub target: Option<String>,
}

#[derive(Deserialize)]
struct TargetListJson {
    #[serde(default)]
    items: Vec<TargetItem>,
}

#[derive(Deserialize, Clone)]
pub struct TargetItem {
    pub selector: String,
    #[serde(default)]
    pub active: bool,
}

#[derive(Deserialize)]
struct SessionList {
    #[serde(default)]
    items: Vec<Instance>,
}

#[derive(Deserialize, Clone)]
pub struct Instance {
    pub instance_id: String,
    #[serde(default)]
    pub binaries: Vec<String>,
    #[serde(default)]
    pub started_at: String,
}

/// One recovered function: display address + name.
#[derive(Clone)]
pub struct Func {
    pub addr: String,
    pub name: String,
}

/// One Binary Ninja local/parameter. Stack variables use a signed frame offset
/// in `storage`; `span_to_next` is the recovered slot span, not the type width.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct LocalVariable {
    #[serde(default)]
    pub local_id: String,
    pub name: String,
    #[serde(rename = "type", default)]
    pub type_name: String,
    #[serde(default)]
    pub source_type: String,
    #[serde(default)]
    pub storage: i64,
    #[serde(default)]
    pub span_to_next: Option<u64>,
    #[serde(default)]
    pub is_parameter: bool,
}

impl LocalVariable {
    pub fn is_stack(&self) -> bool {
        self.source_type == "StackVariableSourceType"
    }

    pub fn is_synthetic(&self) -> bool {
        self.name.starts_with("__") || (self.type_name == "void" && self.name.starts_with("arg_"))
    }
}

#[derive(Deserialize)]
struct LocalListJson {
    #[serde(default)]
    locals: Vec<LocalVariable>,
}

/// A function reference as bn emits it in JSON; we only need the resolved name.
#[derive(Deserialize, Default)]
struct FnRef {
    #[serde(default)]
    name: String,
}

/// The subset of `bn decompile --format json` we consume.
#[derive(Deserialize)]
struct DecompiledFn {
    #[serde(default)]
    text: String,
    #[serde(default)]
    function: FnRef,
}

/// A unique temp path for a `--out` capture (`bn-lens-<pid>-<seq>.out`), so
/// concurrent/sequential captures never share a file (see [`Bn::run_out`]).
fn unique_tmp() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!("bn-lens-{}-{seq}.out", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// Whether a `--summary` mutation payload reports success. The summary is a JSON
/// object even in text mode, so prefer its `success` field; fall back to a
/// substring probe so a format change can't silently read as failure.
fn mutation_ok(out: &str) -> bool {
    for line in out.lines() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if let Some(success) = value.get("success").and_then(serde_json::Value::as_bool) {
                return success;
            }
        }
    }
    out.contains("\"success\": true") || out.contains("\"success\":true")
}

fn parse_local_list_json(text: &str) -> Vec<LocalVariable> {
    serde_json::from_str::<LocalListJson>(text)
        .map(|listing| listing.locals)
        .unwrap_or_default()
}

impl Bn {
    pub fn new(bin: String, instance: Option<String>, target: Option<String>) -> Self {
        Bn {
            bin,
            instance,
            target,
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.bin);
        if let Some(i) = &self.instance {
            c.arg("-i").arg(i);
        }
        if let Some(t) = &self.target {
            c.arg("-t").arg(t);
        }
        c
    }

    /// Targets open in this instance (`-t` selectors).
    pub fn target_list(&self) -> Vec<TargetItem> {
        // listing doesn't need -t; use a bare instance-scoped call
        let mut c = Command::new(&self.bin);
        if let Some(i) = &self.instance {
            c.arg("-i").arg(i);
        }
        let out = c
            .args(["target", "list", "--format", "json"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        serde_json::from_str::<TargetListJson>(&out)
            .map(|t| t.items)
            .unwrap_or_default()
    }

    fn run(&self, args: &[&str]) -> String {
        self.cmd()
            .args(args)
            .output()
            .ok()
            .map(|o| {
                if o.stdout.is_empty() {
                    String::from_utf8_lossy(&o.stderr).into_owned()
                } else {
                    String::from_utf8_lossy(&o.stdout).into_owned()
                }
            })
            .unwrap_or_default()
    }

    /// Run a subcommand capturing the FULL output via `--out` to a temp file —
    /// dodging bn's stdout "spill envelope" for large results (any read that can
    /// exceed ~10k tokens: function list, exports, imports, xrefs, locals on a
    /// big binary).
    ///
    /// The temp path is unique per call (and removed after reading), so a bn
    /// invocation that fails to (re)write the file can't leave us silently
    /// reading a *previous* call's bytes — and the refresh worker thread can't
    /// collide with a foreground read.
    fn run_out(&self, args: &[&str]) -> String {
        let tmp = unique_tmp();
        let _ = self.cmd().args(args).args(["--out", &tmp]).output();
        let out = std::fs::read_to_string(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);
        out
    }

    /// All instances (parsed from `bn session list --format json`), regardless
    /// of the bound instance.
    pub fn session_list(bin: &str) -> Vec<Instance> {
        let out = Command::new(bin)
            .args(["session", "list", "--format", "json"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        serde_json::from_str::<SessionList>(&out)
            .map(|s| s.items)
            .unwrap_or_default()
    }

    /// `bn function list` -> [(addr, name)].
    pub fn functions(&self) -> Vec<Func> {
        let out = self.run_out(&["function", "list", "--limit", "5000"]);
        out.lines()
            .filter_map(|line| {
                let mut it = line.split_whitespace();
                let addr = it.next()?;
                let name = it.next()?;
                if addr.starts_with("0x") {
                    Some(Func {
                        addr: addr.to_string(),
                        name: name.to_string(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    /// Import symbol names (`bn imports`) — the noise to dim in the picker.
    pub fn imports(&self) -> HashSet<String> {
        self.run_out(&["imports"])
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
            .collect()
    }

    /// `bn imports` -> [(addr, name)] for the Imports/attack-surface view.
    pub fn imports_list(&self) -> Vec<(String, String)> {
        self.run_out(&["imports"])
            .lines()
            .filter_map(|line| {
                let mut it = line.split_whitespace();
                let addr = it.next()?;
                let name = it.next()?;
                addr.starts_with("0x")
                    .then(|| (addr.to_string(), name.to_string()))
            })
            .collect()
    }

    /// `bn exports` -> (name->addr map, set of data-symbol names).
    pub fn symbols(&self) -> (HashMap<String, String>, HashSet<String>) {
        let out = self.run_out(&["exports"]);
        let mut addr = HashMap::new();
        let mut data = HashSet::new();
        for line in out.lines() {
            let mut it = line.split_whitespace();
            let (Some(a), Some(n)) = (it.next(), it.next()) else {
                continue;
            };
            if !a.starts_with("0x") {
                continue;
            }
            addr.insert(n.to_string(), a.to_string());
            if line.contains("(data)") {
                data.insert(n.to_string());
            }
        }
        (addr, data)
    }

    /// Decompile via `--out` (avoids the stdout spill envelope for big funcs).
    pub fn decompile(&self, name: &str) -> String {
        let out = self.run_out(&["decompile", name]);
        if out.trim().is_empty() {
            "(no output)".into()
        } else {
            out
        }
    }

    /// Decompile with each pseudo-C line prefixed by its 8-hex address column
    /// (`bn decompile --addresses`), via `--out`. Lets a caller map a use
    /// address back to the statement it belongs to.
    pub fn decompile_addr(&self, name: &str) -> String {
        self.run_out(&["decompile", name, "--addresses"])
    }

    /// Decompile `id` — a function name or any *interior* address (bn resolves
    /// it to the containing function) — as JSON, returning
    /// `(function name, address-prefixed text)`. The JSON gives us the resolved
    /// name directly instead of scraping the text header.
    pub fn decompile_json(&self, id: &str) -> Option<(String, String)> {
        let out = self.run_out(&["decompile", id, "--addresses", "--format", "json"]);
        let parsed: DecompiledFn = serde_json::from_str(&out).ok()?;
        if parsed.text.is_empty() {
            None
        } else {
            Some((parsed.function.name, parsed.text))
        }
    }

    /// IL dump at a given level (`hlil`/`mlil`/`llil`), via `--out`.
    pub fn il(&self, name: &str, level: &str) -> String {
        let out = self.run_out(&["il", name, "--view", level]);
        if out.trim().is_empty() {
            "(no IL)".into()
        } else {
            out
        }
    }

    /// Linear disassembly of a function, via `--out`.
    pub fn disasm(&self, name: &str) -> String {
        let out = self.run_out(&["disasm", name]);
        if out.trim().is_empty() {
            "(no disassembly)".into()
        } else {
            out
        }
    }

    /// `n` instructions in address-linear order starting *exactly* at `addr`
    /// (unlike `--count`, which slices from the containing function's start).
    /// Used to show the single instruction at an xref callsite.
    pub fn disasm_linear(&self, addr: &str, n: usize) -> String {
        let count = n.to_string();
        self.run(&["disasm", addr, "--linear", &count])
    }

    /// Xrefs (small; stdout is fine).
    pub fn xrefs(&self, name: &str) -> String {
        let s = self.run_out(&["xrefs", name]);
        if s.trim().is_empty() {
            "(no xrefs)".into()
        } else {
            s
        }
    }

    /// Raw `bn target info` text (for the switcher preview).
    pub fn target_info_raw(&self) -> String {
        self.run(&["target", "info"])
    }

    /// Section table as plain lines: a `w+x:` summary then
    /// `start-end  size  perms  semantics  name` per section.
    pub fn sections(&self) -> Vec<String> {
        let out = self.run(&["sections"]);
        let v: Vec<String> = out.lines().map(str::to_string).collect();
        if v.is_empty() {
            vec!["(no sections)".into()]
        } else {
            v
        }
    }

    /// Target architecture, parsed from `bn target info`.
    pub fn arch(&self) -> String {
        self.run(&["target", "info"])
            .lines()
            .find_map(|l| l.trim().strip_prefix("arch:").map(|a| a.trim().to_string()))
            .unwrap_or_default()
    }

    /// All strings: (content, address). Content is bn's rendering (same escape
    /// form as the decompile, so it matches a quote-stripped literal directly).
    pub fn strings(&self) -> Vec<(String, String)> {
        let text = self.run_out(&["strings"]);
        let mut out = Vec::new();
        for line in text.lines() {
            let Some(addr) = line.split_whitespace().next() else {
                continue;
            };
            if !addr.starts_with("0x") {
                continue;
            }
            // content is the quoted run at the end of the line
            if let (Some(i), Some(j)) = (line.find('"'), line.rfind('"')) {
                if j > i {
                    out.push((line[i + 1..j].to_string(), addr.to_string()));
                }
            }
        }
        out
    }

    /// Structured locals + params from `bn local list --format json`.
    pub fn local_list(&self, func: &str) -> Vec<LocalVariable> {
        let out = self.run_out(&["local", "list", func, "--format", "json"]);
        parse_local_list_json(&out)
    }

    /// Rename a local (`variable` = name or stable id). Returns true on a verified
    /// write. Mutates the live instance in-memory; persistence to the on-disk
    /// `.bndb` is a separate, deliberate `bn save` (not done per-rename).
    pub fn local_rename(&self, func: &str, old: &str, new: &str) -> bool {
        let out = self.run(&["local", "rename", func, old, new, "--summary"]);
        mutation_ok(&out)
    }

    /// Rename a function symbol (`ident` = name or address). Live in-memory,
    /// like [`local_rename`]; not persisted until a deliberate `bn save`.
    pub fn symbol_rename(&self, ident: &str, new: &str) -> bool {
        let out = self.run(&["rename", ident, new, "--kind", "function", "--summary"]);
        mutation_ok(&out)
    }

    /// Set an address comment (the note shown on that line).
    pub fn comment_set_addr(&self, addr: &str, text: &str) -> bool {
        let out = self.run(&["comment", "set", addr, text, "--summary"]);
        mutation_ok(&out)
    }

    /// Set a function's documentation comment (shown atop the function).
    pub fn comment_set_func(&self, func: &str, text: &str) -> bool {
        let out = self.run(&["comment", "set", "--function", func, text, "--summary"]);
        mutation_ok(&out)
    }

    /// Add a tag of `ty` (e.g. `Bookmarks`) at an address, with optional note.
    pub fn tag_add_addr(&self, addr: &str, ty: &str, data: &str) -> bool {
        let out = self.run(&[
            "tag",
            "add",
            addr,
            "--type",
            ty,
            "--data",
            data,
            "--summary",
        ]);
        mutation_ok(&out)
    }

    /// Add a tag of `ty` on a whole function, with optional note.
    pub fn tag_add_func(&self, func: &str, ty: &str, data: &str) -> bool {
        let out = self.run(&[
            "tag",
            "add",
            "--function",
            func,
            "--type",
            ty,
            "--data",
            data,
            "--summary",
        ]);
        mutation_ok(&out)
    }

    /// Hex+ASCII dump of `length` bytes at `addr`.
    pub fn read(&self, addr: &str, length: usize) -> String {
        let len = length.to_string();
        let s = self.run(&["read", addr, "--length", &len]);
        if s.trim().is_empty() {
            "(no data)".into()
        } else {
            s
        }
    }
}

/// Resolve which bn instance to drive, from the launching pane's cwd.
///
/// Order: `BN_LENS_INSTANCE` -> newest `.bn-<id>` marker in cwd -> single live
/// instance -> newest-started live instance. Markers get GC'd by concurrent
/// sessions, so the live-instance fallbacks matter.
pub fn resolve_instance(bin: &str, cwd: &str) -> Option<String> {
    if let Ok(i) = std::env::var("BN_LENS_INSTANCE") {
        if !i.is_empty() {
            return Some(i);
        }
    }
    if !cwd.is_empty() {
        let mut markers: Vec<(std::time::SystemTime, String)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(cwd) {
            for e in rd.flatten() {
                let fname = e.file_name();
                let fname = fname.to_string_lossy();
                if let Some(id) = fname.strip_prefix(".bn-") {
                    if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                        markers.push((m, id.to_string()));
                    }
                }
            }
        }
        if !markers.is_empty() {
            markers.sort_by(|a, b| b.0.cmp(&a.0));
            return Some(markers[0].1.clone());
        }
    }
    newest_live(bin, None)
}

/// Newest-started live instance that has a binary open (optionally excluding one).
pub fn newest_live(bin: &str, exclude: Option<&str>) -> Option<String> {
    let mut live: Vec<Instance> = Bn::session_list(bin)
        .into_iter()
        .filter(|i| !i.binaries.is_empty() && Some(i.instance_id.as_str()) != exclude)
        .collect();
    if live.len() == 1 {
        return Some(live.remove(0).instance_id);
    }
    live.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    live.into_iter().next().map(|i| i.instance_id)
}

#[cfg(test)]
mod tests {
    use super::parse_local_list_json;

    #[test]
    fn parses_structured_stack_locals_without_losing_types() {
        let json = r#"{
            "function": {"address": "0x401000", "name": "main"},
            "locals": [
                {
                    "local_id": "0x401000:local:stack:-24:0:1",
                    "name": "handler",
                    "type": "int32_t (*)(int32_t, char**)",
                    "source_type": "StackVariableSourceType",
                    "storage": -24,
                    "span_to_next": 16,
                    "is_parameter": false
                },
                {
                    "local_id": "0x401000:param:reg:4:0:2",
                    "name": "argc",
                    "type": "int32_t",
                    "source_type": "RegisterVariableSourceType",
                    "storage": 4,
                    "is_parameter": true
                }
            ]
        }"#;
        let locals = parse_local_list_json(json);
        assert_eq!(locals.len(), 2);
        assert!(locals[0].is_stack());
        assert_eq!(locals[0].storage, -24);
        assert_eq!(locals[0].span_to_next, Some(16));
        assert_eq!(locals[0].type_name, "int32_t (*)(int32_t, char**)");
        assert!(!locals[1].is_stack());
    }
}
