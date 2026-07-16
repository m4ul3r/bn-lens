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

impl Bn {
    pub fn new(bin: String, instance: Option<String>, target: Option<String>) -> Self {
        Bn { bin, instance, target }
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
    /// big binary). Sequential single-threaded use, so one temp file is fine.
    fn run_out(&self, args: &[&str]) -> String {
        let tmp = std::env::temp_dir().join(format!("bn-lens-out-{}.txt", std::process::id()));
        let tmp = tmp.to_string_lossy().into_owned();
        let _ = self.cmd().args(args).args(["--out", &tmp]).output();
        std::fs::read_to_string(&tmp).unwrap_or_default()
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
        let tmp = std::env::temp_dir().join(format!("bn-lens-{}.c", std::process::id()));
        let tmp = tmp.to_string_lossy().into_owned();
        let _ = self
            .cmd()
            .args(["decompile", name, "--out", &tmp])
            .output();
        std::fs::read_to_string(&tmp).unwrap_or_else(|_| "(no output)".into())
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
        let tmp = std::env::temp_dir().join(format!("bn-lens-str-{}.txt", std::process::id()));
        let tmp = tmp.to_string_lossy().into_owned();
        let _ = self.cmd().args(["strings", "--out", &tmp]).output();
        let text = std::fs::read_to_string(&tmp).unwrap_or_default();
        let mut out = Vec::new();
        for line in text.lines() {
            let Some(addr) = line.split_whitespace().next() else { continue };
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

    /// Locals + params of a function: name -> type (from `local list`).
    pub fn local_list(&self, func: &str) -> HashMap<String, String> {
        let out = self.run_out(&["local", "list", func]);
        let mut m = HashMap::new();
        for line in out.lines() {
            // entries are indented ("  name   type   [id: …]"); headers are not
            if !line.starts_with("  ") {
                continue;
            }
            let mut it = line.split_whitespace();
            if let (Some(name), Some(ty)) = (it.next(), it.next()) {
                m.insert(name.to_string(), ty.to_string());
            }
        }
        m
    }

    /// Rename a local (`variable` = name or stable id). Returns true on a verified
    /// write. Mutates the live instance in-memory; persistence to the on-disk
    /// `.bndb` is a separate, deliberate `bn save` (not done per-rename).
    pub fn local_rename(&self, func: &str, old: &str, new: &str) -> bool {
        let out = self.run(&["local", "rename", func, old, new, "--summary"]);
        out.contains("\"success\": true") || out.contains("\"success\":true")
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
