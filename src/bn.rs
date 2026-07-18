//! Thin wrappers over the `bn` CLI plus instance resolution.
//!
//! Big reads (decompile) are captured via `--out <file>` to sidestep bn's
//! stdout "spill envelope" for large output; small reads use stdout directly.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

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
    /// The most recent state-building command failure for this handle. Clones
    /// share it, so a failed context/list refresh is visible to the top-level
    /// status bar instead of being flattened into a plausible empty result.
    /// Per-item viewer reads report errors locally and never poison this state.
    health: Arc<Mutex<Option<CommandFailure>>>,
}

#[derive(Clone)]
struct CommandFailure {
    key: String,
    message: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HealthScope {
    /// The caller renders this command's failure in its own view/popup.
    Local,
    /// Failure means a cached list/context may be stale or incomplete.
    Shared,
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
    #[serde(default)]
    pub analysis_state: String,
}

/// Analysis completeness reported by `bn target info`. Unknown is deliberately
/// not treated as full: absence claims are authoritative only in `Full` state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisState {
    Full,
    Quick,
    Unknown(String),
}

impl AnalysisState {
    pub fn from_raw(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "full" => Self::Full,
            "quick" => Self::Quick,
            _ => Self::Unknown(raw.trim().to_string()),
        }
    }

    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Full)
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Full => "full",
            Self::Quick => "quick",
            Self::Unknown(raw) if !raw.is_empty() => raw,
            Self::Unknown(_) => "unknown",
        }
    }
}

#[derive(Deserialize)]
struct TargetInfoJson {
    #[serde(default)]
    arch: String,
    #[serde(default)]
    analysis_state: String,
}

pub struct TargetInfo {
    pub arch: String,
    pub analysis_state: AnalysisState,
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
    /// Stable identifier passed back to bn (normally `raw_name`).
    pub name: String,
    /// Human-facing demangled/short name.
    pub display_name: String,
}

#[derive(Deserialize)]
struct FunctionListJson {
    #[serde(default)]
    items: Vec<FunctionItemJson>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct FunctionItemJson {
    #[serde(default)]
    address: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    raw_name: String,
    #[serde(default)]
    display_name: String,
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
    #[serde(default)]
    address: String,
}

/// The subset of `bn decompile --format json` we consume.
#[derive(Deserialize)]
struct DecompiledFn {
    #[serde(default)]
    text: String,
    #[serde(default)]
    function: FnRef,
}

/// One annotation for the Marks view — a comment or a tag, unified.
#[derive(Clone)]
pub struct Mark {
    pub addr: String,
    /// `comment`, or the tag type (`Bookmarks`, `Important`, …).
    pub kind: String,
    pub text: String,
    pub func: String,
}

#[derive(Deserialize)]
struct TagListJson {
    #[serde(default)]
    items: Vec<TagItemJson>,
}

// Fields are `Option<String>`, not `#[serde(default)] String`: bn emits an
// explicit `null` (not an absent field) for `function` on a comment/tag outside
// any function, and for `address` on a function-scoped tag. `#[serde(default)]`
// fills only *absent* fields, so a bare `String` would error on `null` and abort
// the whole `Vec` — silently dropping every mark. `Option` absorbs the null.
#[derive(Deserialize)]
struct TagItemJson {
    #[serde(default)]
    address: Option<String>,
    #[serde(default, rename = "type")]
    type_name: Option<String>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    function: Option<String>,
}

#[derive(Deserialize)]
struct CommentListJson {
    #[serde(default)]
    items: Vec<CommentItemJson>,
}

#[derive(Deserialize)]
struct CommentItemJson {
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    function: Option<String>,
}

/// One exported symbol for the Exports view: its address, name, and whether
/// bn tagged it `(data)` (a global) rather than a function.
#[derive(Clone)]
pub struct Export {
    pub addr: String,
    /// Stable identifier passed to bn.
    pub name: String,
    pub display_name: String,
    pub is_data: bool,
}

#[derive(Deserialize)]
struct ExportsJson {
    #[serde(default)]
    items: Vec<ExportJson>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct ExportJson {
    #[serde(default)]
    address: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    raw_name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    kind: String,
}

#[derive(Clone)]
pub struct Import {
    pub addr: String,
    pub name: String,
    pub raw_name: String,
}

#[derive(Deserialize)]
struct ImportsJson {
    #[serde(default)]
    items: Vec<ImportJson>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct ImportJson {
    #[serde(default)]
    address: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    raw_name: String,
}

/// One C++ class recovered by bn's class lens.
#[derive(Clone)]
pub struct ClassItem {
    pub name: String,
    pub method_count: usize,
    pub has_vtable: bool,
    pub confidence: String,
    pub bases: Vec<String>,
}

#[derive(Deserialize)]
struct ClassesJson {
    #[serde(default)]
    items: Vec<ClassItemJson>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct ClassItemJson {
    #[serde(default)]
    name: String,
    #[serde(default)]
    method_count: usize,
    #[serde(default)]
    has_vtable: bool,
    #[serde(default)]
    confidence: String,
    #[serde(default)]
    bases: Vec<String>,
    #[serde(default)]
    artifact: bool,
}

/// Roles assigned by bn's active taint-model catalog to one raw binary symbol.
/// This is a presence catalog, never a vulnerability verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelRoles {
    pub source: bool,
    pub sink_classes: Vec<String>,
    pub propagator: bool,
}

#[derive(Deserialize)]
struct ModelsJson {
    #[serde(default)]
    items: Vec<ModelItemJson>,
}

#[derive(Deserialize)]
struct ModelItemJson {
    #[serde(default)]
    role: String,
    #[serde(default)]
    class: String,
    #[serde(default)]
    raw_symbol: String,
    #[serde(default)]
    resolved_symbol: String,
    #[serde(default)]
    accepted_aliases: Vec<String>,
}

fn recovered_class(item: ClassItemJson) -> Option<ClassItem> {
    if item.name.is_empty() || item.artifact {
        None
    } else {
        Some(ClassItem {
            name: item.name,
            method_count: item.method_count,
            has_vtable: item.has_vtable,
            confidence: item.confidence,
            bases: item.bases,
        })
    }
}

fn roles_from_catalog(catalog: ModelsJson) -> HashMap<String, ModelRoles> {
    let mut roles: HashMap<String, ModelRoles> = HashMap::new();
    for item in catalog.items {
        let mut aliases = item.accepted_aliases;
        aliases.push(item.raw_symbol);
        aliases.push(item.resolved_symbol);
        aliases.retain(|alias| !alias.is_empty());
        aliases.sort();
        aliases.dedup();
        for alias in aliases {
            let role = roles.entry(alias).or_default();
            match item.role.as_str() {
                "source" => role.source = true,
                "sink" => {
                    let class = if item.class.is_empty() {
                        "sink".to_string()
                    } else {
                        item.class.clone()
                    };
                    if !role.sink_classes.contains(&class) {
                        role.sink_classes.push(class);
                        role.sink_classes.sort();
                    }
                }
                "propagator" => role.propagator = true,
                _ => {}
            }
        }
    }
    roles
}

/// One basic block of a function's CFG (from `Bn::cfg`): its start address, the
/// instructions in it, and its outgoing edges.
#[derive(Clone, Deserialize)]
pub struct CfgBlock {
    pub start: String,
    #[serde(default)]
    pub insns: Vec<CfgInsn>,
    #[serde(default)]
    pub edges: Vec<CfgEdge>,
}

#[derive(Clone, Deserialize)]
pub struct CfgInsn {
    /// Instruction address.
    pub a: String,
    /// Rendered disassembly text.
    pub t: String,
}

#[derive(Clone, Deserialize)]
pub struct CfgEdge {
    /// Target block start address.
    pub to: String,
    /// Edge kind (`TrueBranch`/`FalseBranch`/`UnconditionalBranch`/…).
    pub k: String,
}

#[derive(Deserialize)]
struct CfgJson {
    #[serde(default)]
    blocks: Vec<CfgBlock>,
}

/// `bn py exec --format json` wraps a script's `print` output in this envelope;
/// we parse it, then parse the inner JSON the script emitted.
#[derive(Deserialize)]
struct PyEnvelope {
    #[serde(default)]
    stdout: String,
}

/// The `bn py exec` program that walks a function's basic blocks and prints the
/// CFG as JSON. `{IDENT}` is replaced with a single-quote-escaped identifier;
/// `{IL}` with the rendering level (`asm`/`mlil`/`hlil`). For IL levels the
/// block `start`/edge `to` values are IL instruction indexes (hex) — unique
/// block identities, unlike first-line addresses, which collide when one
/// assembly instruction expands to several IL blocks — while each line's `a`
/// stays a real address.
const CFG_PROGRAM: &str = r#"
import json
def _resolve(bv, ident):
    fns = bv.get_functions_by_name(ident)
    if fns:
        return fns[0]
    try:
        addr = int(ident, 16) if ident.lower().startswith('0x') else int(ident)
    except ValueError:
        return None
    f = bv.get_function_at(addr)
    if f:
        return f
    fs = bv.get_functions_containing(addr)
    return fs[0] if fs else None
fn = _resolve(bv, '{IDENT}')
level = '{IL}'
if fn is not None and level != 'asm':
    try:
        fn = fn.mlil if level == 'mlil' else fn.hlil
    except Exception:
        fn = None
blocks = []
if fn is not None:
    for bb in fn.basic_blocks:
        insns = [{'a': hex(l.address), 't': ''.join(str(t) for t in l.tokens)}
                 for l in bb.disassembly_text]
        edges = [{'to': hex(e.target.start), 'k': e.type.name}
                 for e in bb.outgoing_edges if e.target is not None]
        blocks.append({'start': hex(bb.start), 'insns': insns, 'edges': edges})
print(json.dumps({'blocks': blocks}))
"#;

/// One entry in the Types view: a type's name and kind (`struct`/`union`/
/// `enum`/`int`/`named_type_ref`/…).
#[derive(Clone)]
pub struct TypeItem {
    pub name: String,
    pub kind: String,
}

#[derive(Deserialize)]
struct TypesJson {
    #[serde(default)]
    items: Vec<TypeItemJson>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct TypeItemJson {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    decl: String,
}

/// Result of previewing a `types declare` (validate without committing): either
/// the types it would define (name + rendered layout) or the parser error.
pub enum TypeCheck {
    Ok(Vec<TypeLayout>),
    Err(String),
}

/// A previewed/declared type: its name and bn's rendered layout text (the first
/// line carries `// size=0x…`).
pub struct TypeLayout {
    pub name: String,
    pub layout: String,
}

#[derive(Deserialize)]
struct PreviewPayload {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    affected_types: Vec<AffectedTypeJson>,
    #[serde(default)]
    results: Vec<PreviewResultJson>,
}

#[derive(Deserialize)]
struct AffectedTypeJson {
    #[serde(default)]
    name: String,
    #[serde(default)]
    after_layout: String,
}

#[derive(Deserialize)]
struct PreviewResultJson {
    #[serde(default)]
    message: Option<String>,
}

/// A `--summary` mutation payload's fields we consume for `types declare`.
#[derive(Deserialize)]
struct MutationSummaryJson {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    changed_count: u64,
    #[serde(default)]
    first_error: Option<String>,
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

/// Tidy a bn declaration-parser error for a one-line status: collapse
/// whitespace/newlines and cap the length so a long multi-error blob stays
/// readable in the editor footer.
fn clean_err(msg: &str) -> String {
    let collapsed = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > 160 {
        let head: String = collapsed.chars().take(159).collect();
        format!("{head}…")
    } else {
        collapsed
    }
}

/// Parse one `bn types --format json` page into items plus the envelope's
/// `has_more` flag. bn reports actual unions as `kind: "struct"`; the `decl`
/// field (`union __BLOB_ARG` vs `struct S`) disambiguates, so recover the
/// `union` kind from it here.
fn parse_types_page(text: &str) -> (Vec<TypeItem>, bool) {
    match serde_json::from_str::<TypesJson>(text.trim()) {
        Ok(page) => {
            let items = page
                .items
                .into_iter()
                .map(|i| {
                    let kind = if i.decl.split_whitespace().next() == Some("union") {
                        "union".to_string()
                    } else {
                        i.kind
                    };
                    TypeItem { name: i.name, kind }
                })
                .collect();
            (items, page.has_more)
        }
        Err(_) => (Vec::new(), false),
    }
}

fn parse_local_list_json(text: &str) -> Vec<LocalVariable> {
    serde_json::from_str::<LocalListJson>(text)
        .map(|listing| listing.locals)
        .unwrap_or_default()
}

/// Push a [`Mark`] from optional JSON fields, keeping it if it has *either* an
/// address or a containing function (a function-scoped tag has a null address;
/// a data-address comment has a null function). Fully-empty entries are dropped.
fn push_mark(
    marks: &mut Vec<Mark>,
    addr: Option<String>,
    kind: String,
    text: Option<String>,
    func: Option<String>,
) {
    let addr = addr.unwrap_or_default();
    let func = func.unwrap_or_default();
    if addr.is_empty() && func.is_empty() {
        return;
    }
    marks.push(Mark {
        addr,
        kind,
        text: text.unwrap_or_default(),
        func,
    });
}

impl Bn {
    pub fn new(bin: String, instance: Option<String>, target: Option<String>) -> Self {
        Bn {
            bin,
            instance,
            target,
            health: Arc::new(Mutex::new(None)),
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

    fn command_key(args: &[&str]) -> String {
        args.first()
            .map(|first| {
                if let Some(second) = args.get(1).filter(|arg| !arg.starts_with('-')) {
                    format!("{first} {second}")
                } else {
                    (*first).to_string()
                }
            })
            .unwrap_or_else(|| "bn".into())
    }

    fn record_failure(&self, args: &[&str], message: String) {
        if let Ok(mut health) = self.health.lock() {
            *health = Some(CommandFailure {
                key: Self::command_key(args),
                message,
            });
        }
    }

    /// A successful retry heals the same failed command. An unrelated success
    /// must not erase evidence that another view is stale or incomplete.
    fn record_success(&self, args: &[&str]) {
        let key = Self::command_key(args);
        if let Ok(mut health) = self.health.lock() {
            if health.as_ref().is_some_and(|failure| failure.key == key) {
                *health = None;
            }
        }
    }

    /// Most recent state-building command failure for this shared handle. A
    /// successful retry of that state family clears it; item reads neither set
    /// nor clear shared health.
    pub fn last_error(&self) -> Option<String> {
        self.health
            .lock()
            .ok()
            .and_then(|health| health.as_ref().map(|failure| failure.message.clone()))
    }

    fn command_error(&self, args: &[&str], stderr: &[u8], stdout: &[u8]) -> String {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        let detail = clean_err(&String::from_utf8_lossy(detail));
        let command = Self::command_key(args);
        if detail.is_empty() {
            format!("bn {command} failed")
        } else {
            format!("bn {command}: {detail}")
        }
    }

    fn run_checked_with_scope(&self, args: &[&str], scope: HealthScope) -> Result<String, String> {
        let output = self.cmd().args(args).output().map_err(|error| {
            let message = format!("could not start bn: {error}");
            if scope == HealthScope::Shared {
                self.record_failure(args, message.clone());
            }
            message
        })?;
        if !output.status.success() {
            let message = self.command_error(args, &output.stderr, &output.stdout);
            if scope == HealthScope::Shared {
                self.record_failure(args, message.clone());
            }
            return Err(message);
        }
        if scope == HealthScope::Shared {
            self.record_success(args);
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn run_checked(&self, args: &[&str]) -> Result<String, String> {
        self.run_checked_with_scope(args, HealthScope::Local)
    }

    fn run_state_checked(&self, args: &[&str]) -> Result<String, String> {
        self.run_checked_with_scope(args, HealthScope::Shared)
    }

    fn run_out_checked_with_scope(
        &self,
        args: &[&str],
        scope: HealthScope,
    ) -> Result<String, String> {
        let tmp = unique_tmp();
        let output = self
            .cmd()
            .args(args)
            .args(["--out", &tmp])
            .output()
            .map_err(|error| {
                let message = format!("could not start bn: {error}");
                if scope == HealthScope::Shared {
                    self.record_failure(args, message.clone());
                }
                message
            })?;
        let captured = std::fs::read_to_string(&tmp);
        let _ = std::fs::remove_file(&tmp);
        if !output.status.success() {
            let message = self.command_error(args, &output.stderr, &output.stdout);
            if scope == HealthScope::Shared {
                self.record_failure(args, message.clone());
            }
            return Err(message);
        }
        let captured = captured.map_err(|error| {
            let command = Self::command_key(args);
            let message = format!("could not read bn {command} output: {error}");
            if scope == HealthScope::Shared {
                self.record_failure(args, message.clone());
            }
            message
        })?;
        if scope == HealthScope::Shared {
            self.record_success(args);
        }
        Ok(captured)
    }

    fn run_out_checked(&self, args: &[&str]) -> Result<String, String> {
        self.run_out_checked_with_scope(args, HealthScope::Local)
    }

    fn run_out_state_checked(&self, args: &[&str]) -> Result<String, String> {
        self.run_out_checked_with_scope(args, HealthScope::Shared)
    }

    /// Targets open in this instance (`-t` selectors).
    pub fn target_list(&self) -> Vec<TargetItem> {
        self.target_list_checked().unwrap_or_default()
    }

    pub fn target_list_checked(&self) -> Result<Vec<TargetItem>, String> {
        // listing doesn't need -t; use a bare instance-scoped call
        let mut c = Command::new(&self.bin);
        if let Some(i) = &self.instance {
            c.arg("-i").arg(i);
        }
        let out = match c.args(["target", "list", "--format", "json"]).output() {
            Ok(output) if output.status.success() => {
                self.record_success(&["target", "list"]);
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
            Ok(output) => {
                let message =
                    self.command_error(&["target", "list"], &output.stderr, &output.stdout);
                self.record_failure(&["target", "list"], message.clone());
                return Err(message);
            }
            Err(error) => {
                let message = format!("could not start bn: {error}");
                self.record_failure(&["target", "list"], message.clone());
                return Err(message);
            }
        };
        serde_json::from_str::<TargetListJson>(&out)
            .map(|targets| targets.items)
            .map_err(|error| {
                let message = format!("bn target list returned invalid JSON: {error}");
                self.record_failure(&["target", "list"], message.clone());
                message
            })
    }

    fn run(&self, args: &[&str]) -> String {
        self.run_checked(args)
            .unwrap_or_else(|error| format!("✗ {error}"))
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
        self.run_out_checked(args)
            .unwrap_or_else(|error| format!("✗ {error}"))
    }

    /// State-building counterpart to [`Self::run_out`]. Only list/context
    /// reads use it: their failures invalidate cached absence/count claims and
    /// therefore belong in the shared backend-health banner.
    fn run_out_state(&self, args: &[&str]) -> String {
        self.run_out_state_checked(args)
            .unwrap_or_else(|error| format!("✗ {error}"))
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

    /// Full JSON-backed function inventory. `display_name` preserves BN's
    /// demangling while `name` remains a stable command identifier.
    pub fn functions_checked(&self) -> Result<Vec<Func>, String> {
        let mut all = Vec::new();
        let mut offset = 0usize;
        loop {
            let offset_arg = offset.to_string();
            let out = self.run_out_state_checked(&[
                "function",
                "list",
                "--offset",
                &offset_arg,
                "--limit",
                "5000",
                "--format",
                "json",
            ])?;
            let page: FunctionListJson = serde_json::from_str(out.trim()).map_err(|error| {
                let message = format!("bn function list returned invalid JSON: {error}");
                self.record_failure(&["function", "list"], message.clone());
                message
            })?;
            let returned = page.items.len();
            offset += returned;
            all.extend(page.items.into_iter().filter_map(|item| {
                if !item.address.starts_with("0x") {
                    return None;
                }
                let name = if item.raw_name.is_empty() {
                    item.name.clone()
                } else {
                    item.raw_name
                };
                let display_name = if item.display_name.is_empty() {
                    item.name
                } else {
                    item.display_name
                };
                Some(Func {
                    addr: item.address,
                    name,
                    display_name,
                })
            }));
            if !page.has_more || returned == 0 {
                break;
            }
        }
        Ok(all)
    }

    /// Import symbol aliases (`name` + `raw_name`) — used to dim PLT entries and
    /// to refuse accidental import renames regardless of display spelling.
    pub fn imports_checked(&self) -> Result<HashSet<String>, String> {
        let mut names = HashSet::new();
        for import in self.imports_list_checked()? {
            names.insert(import.name);
            names.insert(import.raw_name);
        }
        names.remove("");
        Ok(names)
    }

    /// Every annotation — comments + tags — merged for the Marks view. Both come
    /// from `bn`'s JSON so we get the containing function and (for tags) the type
    /// without scraping text.
    pub fn marks(&self) -> Vec<Mark> {
        let mut marks = Vec::new();
        let comments = self.run_out_state(&["comment", "list", "--format", "json"]);
        if let Ok(list) = serde_json::from_str::<CommentListJson>(&comments) {
            for c in list.items {
                push_mark(
                    &mut marks,
                    c.address,
                    "comment".into(),
                    c.comment,
                    c.function,
                );
            }
        }
        let tags = self.run_out_state(&["tag", "list", "--format", "json"]);
        if let Ok(list) = serde_json::from_str::<TagListJson>(&tags) {
            for t in list.items {
                let kind = t
                    .type_name
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "tag".into());
                push_mark(&mut marks, t.address, kind, t.data, t.function);
            }
        }
        marks
    }

    /// JSON-backed imports for the attack-surface view.
    pub fn imports_list_checked(&self) -> Result<Vec<Import>, String> {
        let mut all = Vec::new();
        let mut offset = 0usize;
        loop {
            let offset_arg = offset.to_string();
            let out = self.run_out_state_checked(&[
                "imports",
                "--offset",
                &offset_arg,
                "--limit",
                "5000",
                "--format",
                "json",
            ])?;
            let page: ImportsJson = serde_json::from_str(out.trim()).map_err(|error| {
                let message = format!("bn imports returned invalid JSON: {error}");
                self.record_failure(&["imports"], message.clone());
                message
            })?;
            let returned = page.items.len();
            offset += returned;
            all.extend(page.items.into_iter().filter_map(|item| {
                item.address.starts_with("0x").then(|| Import {
                    addr: item.address,
                    raw_name: if item.raw_name.is_empty() {
                        item.name.clone()
                    } else {
                        item.raw_name
                    },
                    name: item.name,
                })
            }));
            if !page.has_more || returned == 0 {
                break;
            }
        }
        Ok(all)
    }

    pub fn imports_list(&self) -> Vec<Import> {
        self.imports_list_checked().unwrap_or_default()
    }

    /// Active taint-model roles keyed by every accepted raw symbol spelling.
    /// These are catalog/presence facts, not taint findings.
    pub fn model_roles_present(&self) -> Result<HashMap<String, ModelRoles>, String> {
        let out =
            self.run_out_state_checked(&["taint", "models", "--present", "--format", "json"])?;
        let catalog: ModelsJson = serde_json::from_str(out.trim()).map_err(|error| {
            let message = format!("bn taint models returned invalid JSON: {error}");
            self.record_failure(&["taint", "models"], message.clone());
            message
        })?;
        Ok(roles_from_catalog(catalog))
    }

    /// `bn types` -> the full type list (name + kind) for the Types view,
    /// paged with `--offset` until the envelope reports `has_more: false` so a
    /// database past the page size is never silently truncated.
    pub fn types_list(&self) -> Vec<TypeItem> {
        let mut all: Vec<TypeItem> = Vec::new();
        loop {
            let offset = all.len().to_string();
            let out = self.run_out_state(&[
                "types", "--offset", &offset, "--limit", "5000", "--format", "json",
            ]);
            let (mut items, has_more) = parse_types_page(&out);
            // An empty page also ends the loop, so a malformed/stuck envelope
            // can't page forever.
            if items.is_empty() {
                break;
            }
            all.append(&mut items);
            if !has_more {
                break;
            }
        }
        all
    }

    /// `bn types show <name>` -> the rendered layout (struct fields + offsets).
    pub fn type_show(&self, name: &str) -> Vec<String> {
        let out = self.run_out(&["types", "show", name]);
        let lines: Vec<String> = out.lines().map(str::to_string).collect();
        if lines.is_empty() {
            vec![format!("(no layout for {name})")]
        } else {
            lines
        }
    }

    /// Validate a C declaration without committing (`types declare --preview`):
    /// the resulting type layouts, or the parser error.
    pub fn type_declare_check(&self, decl: &str) -> TypeCheck {
        let out = self.run_out(&["types", "declare", "--preview", "--format", "json", decl]);
        match serde_json::from_str::<PreviewPayload>(out.trim()) {
            Ok(payload) if payload.ok => TypeCheck::Ok(
                payload
                    .affected_types
                    .into_iter()
                    .map(|a| TypeLayout {
                        name: a.name,
                        layout: a.after_layout,
                    })
                    .collect(),
            ),
            Ok(payload) => TypeCheck::Err(clean_err(
                &payload
                    .results
                    .into_iter()
                    .find_map(|r| r.message)
                    .unwrap_or_else(|| "declaration rejected".into()),
            )),
            Err(_) => TypeCheck::Err("no response from bn".into()),
        }
    }

    /// Commit a C declaration as user types (`types declare`). Live in the bn
    /// instance, like every other write — not persisted until an explicit
    /// `bn save`. Returns the count of defined types, or the parser error.
    pub fn type_declare(&self, decl: &str) -> Result<u64, String> {
        let out = self.run_out(&["types", "declare", "--summary", "--format", "json", decl]);
        match serde_json::from_str::<MutationSummaryJson>(out.trim()) {
            Ok(summary) if summary.success => Ok(summary.changed_count),
            Ok(summary) => Err(summary
                .first_error
                .map(|e| clean_err(&e))
                .unwrap_or_else(|| "declaration failed".into())),
            Err(_) => Err("no response from bn".into()),
        }
    }

    /// Domain C++ classes recovered from symbols/RTTI, with STL and vendored
    /// library clusters folded out so firmware classes lead the view.
    pub fn classes_list(&self) -> Result<Vec<ClassItem>, String> {
        let mut all = Vec::new();
        let mut offset = 0usize;
        loop {
            let offset_arg = offset.to_string();
            let out = self.run_out_state_checked(&[
                "class",
                "list",
                "--no-stl",
                "--no-vendor",
                "--offset",
                &offset_arg,
                "--limit",
                "5000",
                "--format",
                "json",
            ])?;
            let page: ClassesJson = serde_json::from_str(out.trim()).map_err(|error| {
                let message = format!("bn class list returned invalid JSON: {error}");
                self.record_failure(&["class", "list"], message.clone());
                message
            })?;
            let returned = page.items.len();
            offset += returned;
            all.extend(page.items.into_iter().filter_map(recovered_class));
            if !page.has_more || returned == 0 {
                break;
            }
        }
        Ok(all)
    }

    pub fn class_show(&self, name: &str) -> Vec<String> {
        match self.run_out_checked(&["class", "show", name]) {
            Ok(out) if !out.trim().is_empty() => out.lines().map(str::to_string).collect(),
            Ok(_) => vec![format!("(no class evidence for {name})")],
            Err(error) => vec![format!("✗ {error}")],
        }
    }

    /// JSON-backed exported symbols (public API), including demangled display
    /// names and an explicit function/data kind.
    pub fn exports_list_checked(&self) -> Result<Vec<Export>, String> {
        let mut all = Vec::new();
        let mut offset = 0usize;
        loop {
            let offset_arg = offset.to_string();
            let out = self.run_out_state_checked(&[
                "exports",
                "--offset",
                &offset_arg,
                "--limit",
                "5000",
                "--format",
                "json",
            ])?;
            let page: ExportsJson = serde_json::from_str(out.trim()).map_err(|error| {
                let message = format!("bn exports returned invalid JSON: {error}");
                self.record_failure(&["exports"], message.clone());
                message
            })?;
            let returned = page.items.len();
            offset += returned;
            all.extend(page.items.into_iter().filter_map(|item| {
                if !item.address.starts_with("0x") {
                    return None;
                }
                let name = if item.raw_name.is_empty() {
                    item.name.clone()
                } else {
                    item.raw_name
                };
                let display_name = if item.display_name.is_empty() {
                    item.name
                } else {
                    item.display_name
                };
                Some(Export {
                    addr: item.address,
                    name,
                    display_name,
                    is_data: item.kind == "data",
                })
            }));
            if !page.has_more || returned == 0 {
                break;
            }
        }
        Ok(all)
    }

    pub fn exports_list(&self) -> Vec<Export> {
        self.exports_list_checked().unwrap_or_default()
    }

    /// Basic blocks + typed edges of `ident`'s control-flow graph at rendering
    /// level `il` (`asm`/`mlil`/`hlil`), via `bn py exec` (there is no
    /// first-class CFG command). Empty on any failure (unknown function, no
    /// blocks, IL unavailable, malformed output) — the caller shows a note.
    pub fn cfg(&self, ident: &str, il: &str) -> Vec<CfgBlock> {
        let escaped = ident.replace('\\', "\\\\").replace('\'', "\\'");
        let program = CFG_PROGRAM.replace("{IDENT}", &escaped).replace("{IL}", il);
        let out = self.run_out(&["py", "exec", "--format", "json", "--code", &program]);
        serde_json::from_str::<PyEnvelope>(&out)
            .ok()
            .and_then(|env| serde_json::from_str::<CfgJson>(&env.stdout).ok())
            .map(|cfg| cfg.blocks)
            .unwrap_or_default()
    }

    /// Export aliases -> address, plus every data-symbol alias.
    pub fn symbols_checked(&self) -> Result<(HashMap<String, String>, HashSet<String>), String> {
        let mut addr = HashMap::new();
        let mut data = HashSet::new();
        for export in self.exports_list_checked()? {
            let aliases = [export.name.clone(), export.display_name.clone()];
            for alias in aliases.into_iter().filter(|alias| !alias.is_empty()) {
                addr.insert(alias.clone(), export.addr.clone());
                if export.is_data {
                    data.insert(alias);
                }
            }
        }
        Ok((addr, data))
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
    /// `(function name, entry address, address-prefixed text)`. The JSON gives
    /// us the resolved identity directly instead of scraping the text header.
    pub fn decompile_json(&self, id: &str) -> Option<(String, String, String)> {
        let out = self.run_out(&["decompile", id, "--addresses", "--format", "json"]);
        let parsed: DecompiledFn = serde_json::from_str(&out).ok()?;
        if parsed.text.is_empty() {
            None
        } else {
            Some((parsed.function.name, parsed.function.address, parsed.text))
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
        self.xrefs_checked(name)
            .unwrap_or_else(|error| format!("✗ {error}"))
    }

    pub fn xrefs_checked(&self, name: &str) -> Result<String, String> {
        let s = self.run_out_checked(&["xrefs", name])?;
        if s.trim().is_empty() {
            Ok("(no xrefs)".into())
        } else {
            Ok(s)
        }
    }

    /// Raw `bn target info` text (for the switcher preview).
    pub fn target_info_raw(&self) -> String {
        self.run(&["target", "info"])
    }

    /// Structured target provenance used by every normal-view header. Failure
    /// is fatal to a context build: silently guessing "full" would make empty
    /// lists unsafe evidence.
    pub fn target_info(&self) -> Result<TargetInfo, String> {
        let out = self.run_state_checked(&["target", "info", "--format", "json"])?;
        let info: TargetInfoJson = serde_json::from_str(out.trim()).map_err(|error| {
            let message = format!("bn target info returned invalid JSON: {error}");
            self.record_failure(&["target", "info"], message.clone());
            message
        })?;
        Ok(TargetInfo {
            arch: info.arch,
            analysis_state: AnalysisState::from_raw(&info.analysis_state),
        })
    }

    /// Section table as plain lines: a `w+x:` summary then
    /// `start-end  size  perms  semantics  name` per section.
    pub fn sections_checked(&self) -> Result<Vec<String>, String> {
        let out = self.run_state_checked(&["sections"])?;
        Ok(out.lines().map(str::to_string).collect())
    }

    pub fn sections(&self) -> Vec<String> {
        match self.sections_checked() {
            Ok(lines) if !lines.is_empty() => lines,
            Ok(_) => vec!["(no sections)".into()],
            Err(error) => vec![format!("✗ {error}")],
        }
    }

    /// All strings: (content, address). Content is bn's rendering (same escape
    /// form as the decompile, so it matches a quote-stripped literal directly).
    pub fn strings(&self) -> Vec<(String, String)> {
        let text = self.run_out_state(&["strings"]);
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
    use super::{
        parse_local_list_json, parse_types_page, push_mark, recovered_class, roles_from_catalog,
        AnalysisState, Bn, ClassItemJson, CommentListJson, ModelsJson, TagListJson,
    };

    #[test]
    fn analysis_state_fails_closed_for_unknown_values() {
        assert!(AnalysisState::from_raw("full").is_complete());
        assert!(!AnalysisState::from_raw("quick").is_complete());
        assert_eq!(AnalysisState::from_raw("").label(), "unknown");
        assert_eq!(AnalysisState::from_raw("partial").label(), "partial");
    }

    #[test]
    fn unrelated_success_does_not_hide_a_command_failure() {
        let bn = Bn::new("bn".into(), None, None);
        bn.record_failure(&["strings"], "strings failed".into());
        bn.record_success(&["class", "list"]);
        assert_eq!(bn.last_error().as_deref(), Some("strings failed"));
        bn.record_success(&["strings"]);
        assert_eq!(bn.last_error(), None);
        assert_eq!(Bn::command_key(&["imports", "--offset", "0"]), "imports");
    }

    #[test]
    fn per_item_reads_never_poison_or_heal_shared_health() {
        let mut bn = Bn::new("/definitely/missing/bn-lens-test-binary".into(), None, None);

        // A failed viewer read is rendered in that viewer, not the global bar.
        assert!(bn.run_out_checked(&["decompile", "missing_fn"]).is_err());
        assert_eq!(bn.last_error(), None);

        // A failed state build is sticky because cached absence/count claims
        // can no longer be trusted.
        assert!(bn.run_out_state_checked(&["function", "list"]).is_err());
        assert!(bn.last_error().is_some());

        // Neither a successful read of the same item nor another item heals a
        // state failure. Only a successful retry of that state family does.
        bn.bin = "/bin/true".into();
        assert!(bn.run_checked(&["decompile", "missing_fn"]).is_ok());
        assert!(bn.last_error().is_some());
        assert!(bn.run_state_checked(&["function", "list"]).is_ok());
        assert_eq!(bn.last_error(), None);
    }

    #[test]
    fn model_roles_merge_aliases_without_calling_presence_findings() {
        let json = r#"{"items":[
          {"role":"source","raw_symbol":"read","resolved_symbol":"read","accepted_aliases":["read"]},
          {"role":"sink","class":"overflow_len","raw_symbol":"read","resolved_symbol":"read","accepted_aliases":["read"]},
          {"role":"propagator","raw_symbol":"memcpy","accepted_aliases":["__memcpy_chk"]}
        ]}"#;
        let catalog = serde_json::from_str::<ModelsJson>(json).expect("model catalog");
        let roles = roles_from_catalog(catalog);
        assert!(roles["read"].source);
        assert_eq!(roles["read"].sink_classes, ["overflow_len"]);
        assert!(roles["memcpy"].propagator);
        assert!(roles["__memcpy_chk"].propagator);
    }

    #[test]
    fn non_class_rtti_artifacts_are_folded_from_the_class_view() {
        let artifact = serde_json::from_str::<ClassItemJson>(
            r#"{"name":"int32_t ()","confidence":"rtti","artifact":true}"#,
        )
        .expect("artifact row");
        assert!(recovered_class(artifact).is_none());

        let domain = serde_json::from_str::<ClassItemJson>(
            r#"{"name":"media::Parser","method_count":4,"has_vtable":true,"confidence":"rtti","bases":["media::Base"],"artifact":false}"#,
        )
        .expect("domain row");
        let domain = recovered_class(domain).expect("real class retained");
        assert_eq!(domain.name, "media::Parser");
        assert_eq!(domain.bases, ["media::Base"]);
    }

    #[test]
    fn types_page_recovers_union_kind_from_decl() {
        // bn reports actual unions as kind:"struct"; only `decl` disambiguates.
        let json = r#"{"items":[
            {"name":"__BLOB_ARG","kind":"struct","decl":"union __BLOB_ARG"},
            {"name":"widget","kind":"struct","decl":"struct widget"},
            {"name":"color","kind":"enum","decl":"enum color"},
            {"name":"unionizer_t","kind":"named_type_ref","decl":"typedef struct widget unionizer_t"}
        ],"has_more":false,"total":4,"returned":4}"#;
        let (items, has_more) = parse_types_page(json);
        assert!(!has_more);
        let kinds: Vec<&str> = items.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, ["union", "struct", "enum", "named_type_ref"]);
        assert_eq!(items[0].name, "__BLOB_ARG");
    }

    #[test]
    fn types_page_reports_more_pages_and_survives_garbage() {
        let json = r#"{"items":[{"name":"widget","kind":"struct","decl":"struct widget"}],
                       "has_more":true,"total":9001,"returned":1}"#;
        let (items, has_more) = parse_types_page(json);
        assert_eq!(items.len(), 1);
        assert!(has_more, "has_more must surface so the caller keeps paging");

        let (items, has_more) = parse_types_page("not json at all");
        assert!(items.is_empty());
        assert!(!has_more, "a malformed page must not page forever");
    }

    #[test]
    fn mark_json_survives_null_function_and_address() {
        // A comment on a data address serializes `"function": null`; a
        // function-scoped tag serializes `"address": null`. Neither may abort
        // deserialization of the whole list.
        let comments = r#"{"items":[
            {"address":"0x1000","comment":"in a func","function":"parse"},
            {"address":"0x4152a0","comment":"on a global","function":null}
        ]}"#;
        let c = serde_json::from_str::<CommentListJson>(comments).expect("null function parses");
        let mut marks = Vec::new();
        for it in c.items {
            push_mark(
                &mut marks,
                it.address,
                "comment".into(),
                it.comment,
                it.function,
            );
        }
        assert_eq!(marks.len(), 2, "both comments kept despite a null function");
        assert_eq!(marks[1].func, ""); // null → empty, still navigable by address

        let tags = r#"{"items":[
            {"address":null,"type":"Bookmarks","data":"whole fn","function":"parse"},
            {"address":"0x2000","type":"Important","data":null,"function":null}
        ]}"#;
        let t = serde_json::from_str::<TagListJson>(tags).expect("null address/data parses");
        let mut tmarks = Vec::new();
        for it in t.items {
            let kind = it
                .type_name
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "tag".into());
            push_mark(&mut tmarks, it.address, kind, it.data, it.function);
        }
        assert_eq!(tmarks.len(), 2);
        assert_eq!(tmarks[0].addr, ""); // function-scoped tag: null address, kept via function
        assert_eq!(tmarks[0].func, "parse");
    }

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
