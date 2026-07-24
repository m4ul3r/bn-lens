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

/// A bn handle bound to a resolved binary path + instance + target.
///
/// Reads and writes go over the bridge socket ([`crate::bnsock`]) whenever one
/// could be resolved, which removes the ~127 ms Python startup each CLI spawn
/// costs (`DESIGN_BN_INTERFACE.md` §2). The `bn` CLI is retained as a fallback for
/// the case where no live registry could be read — an unreadable cache dir or a
/// bridge that registered after we resolved — so the lens degrades to its previous
/// behavior instead of failing outright.
#[derive(Clone)]
pub struct Bn {
    pub bin: String,
    pub instance: Option<String>,
    pub target: Option<String>,
    /// Socket client for this instance, when one is live. `None` means every call
    /// falls back to spawning `bn`.
    client: Option<Arc<crate::bnsock::Client>>,
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

/// One recovered function: display address + name, plus the size/complexity
/// facts BN already returns in the function list (rendered as picker columns).
#[derive(Clone)]
pub struct Func {
    pub addr: String,
    /// Stable identifier passed back to bn (normally `raw_name`).
    pub name: String,
    /// Human-facing demangled/short name.
    pub display_name: String,
    /// Byte length of the function body.
    pub size: u64,
    /// Basic-block count (a cheap branching/complexity proxy).
    pub basic_block_count: u32,
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
    #[serde(default)]
    size: u64,
    #[serde(default)]
    basic_block_count: u32,
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

/// One `bn decompile --addresses` result, split into the pieces a caller needs to
/// render it *and* index it by address — see [`Bn::decompile_read`].
pub struct DecompileRead {
    /// The resolved function's name as bn reports it (an interior address
    /// resolves to its containing function).
    pub name: String,
    /// The resolved function's entry address (`0x…`), possibly empty.
    pub entry: String,
    /// Address-prefixed pseudo-C: every real line led by its 8-hex address
    /// column (parse with [`crate::decomp::dec_lines`]).
    pub text: String,
    /// The `// bn: …` interior-resolution note, without its trailing newline.
    /// `None` for an exact-start or by-name read (the common case).
    pub note: Option<String>,
    /// `warning: …` lines the CLI's text mode appends below the body — the
    /// thunk/veneer disclosure, the analysis-stub warning, the pseudo-C
    /// degradation notice. Load-bearing on a live target; never dropped.
    pub warnings: Vec<String>,
}

/// A function's existing comments, split by where BN stores them: its dedicated
/// `function_doc` string and any comment at the entry `address`. Both render
/// atop the function; `;` edits whichever is present.
#[derive(Clone, Default)]
pub struct FuncComment {
    pub doc: String,
    pub entry_addr: String,
    pub entry_comment: Option<String>,
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

/// One typed data variable recovered from BN's `data_vars`, with its address,
/// symbol (if named), type, current value, and — for pointers — the target it
/// points at (symbolized, or a string preview). The building block of the
/// structured data-section view: a data address reads as a struct field.
#[derive(Clone, Deserialize)]
pub struct DataVar {
    #[serde(rename = "a")]
    pub addr: String,
    #[serde(rename = "n", default)]
    pub name: String,
    #[serde(rename = "t", default)]
    pub type_name: String,
    #[serde(rename = "w", default)]
    pub width: u64,
    /// Decoded value for scalars ≤ 8 bytes (unsigned); `None` for aggregates.
    #[serde(rename = "v", default)]
    pub value: Option<i64>,
    /// Pointer target address (`0x…`), when the type is a pointer.
    #[serde(rename = "p", default)]
    pub ptr: Option<String>,
    /// Symbol at the pointer target, if any.
    #[serde(rename = "ps", default)]
    pub ptr_sym: Option<String>,
    /// ASCII string at the pointer target, if any (preview, truncated).
    #[serde(rename = "pstr", default)]
    pub ptr_str: Option<String>,
    /// Section name the variable lives in (for boundary headers).
    #[serde(rename = "sec", default)]
    pub section: String,
}

#[derive(Deserialize)]
struct DataMapJson {
    #[serde(default)]
    vars: Vec<DataVar>,
}

#[derive(Deserialize)]
struct DataSym {
    a: String,
    n: String,
}

#[derive(Deserialize)]
struct DataSymsJson {
    #[serde(default)]
    syms: Vec<DataSym>,
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

/// The `bn py exec` program that enumerates BN's typed data variables in the
/// half-open address window `[{LO}, {HI})` and prints them as JSON. Each var
/// carries its address, symbol, type string, width, decoded scalar value, and —
/// for pointer types — the target (symbolized, or a short ASCII preview) and its
/// section. Bounded to 400 rows so a huge `.rodata` window can't flood output.
const DATA_MAP_PROGRAM: &str = r#"
import json
lo = int('{LO}', 16); hi = int('{HI}', 16)
psz = bv.address_size
mask = (1 << (psz * 8)) - 1
out = []
for a in sorted(bv.data_vars):
    if a < lo or a >= hi:
        continue
    dv = bv.data_vars[a]
    t = dv.type
    try:
        w = int(t.width)
    except Exception:
        w = 0
    sym = bv.get_symbol_at(a)
    ts = str(t)
    row = {'a': hex(a), 'n': sym.name if sym else '', 't': ts, 'w': w}
    secs = bv.get_sections_at(a)
    if secs:
        row['sec'] = secs[0].name
    try:
        # Treat only a pointer-*sized* pointer type as a single pointer. An
        # array of pointers (e.g. `void* [8]`, width = 8*psz) also contains '*'
        # but must NOT collapse to its first element — leave it untyped so the
        # renderer shows the whole array as bytes instead of hiding elements.
        if '*' in ts and w == psz:
            tgt = bv.read_int(a, psz) & mask
            row['p'] = hex(tgt)
            tsym = bv.get_symbol_at(tgt)
            if tsym:
                row['ps'] = tsym.name
            else:
                s = bv.get_ascii_string_at(tgt, 2)
                if s:
                    row['pstr'] = s.value[:48]
        elif '*' not in ts and 0 < w <= 8:
            row['v'] = bv.read_int(a, w)
    except Exception:
        pass
    out.append(row)
    if len(out) >= 400:
        break
print(json.dumps({'vars': out}))
"#;

/// The `bn py exec` program that lists every *named* data symbol (address +
/// name), including internal ones the exports list omits — so a renamed data
/// global stays interactive. Degrades to an empty list if the enum import fails.
const DATA_SYMBOLS_PROGRAM: &str = r#"
import json
try:
    from binaryninja import SymbolType
    syms = bv.get_symbols_of_type(SymbolType.DataSymbol)
except Exception:
    syms = []
out = []
for s in syms:
    if s.name:
        out.append({'a': hex(s.address), 'n': s.name})
print(json.dumps({'syms': out}))
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
        Ok(page) => types_page(page),
        Err(_) => (Vec::new(), false),
    }
}

/// Shared by the socket and CLI paths so the union-recovery rule lives in one place.
fn types_page(page: TypesJson) -> (Vec<TypeItem>, bool) {
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
        let client = resolve_client(instance.as_deref()).map(Arc::new);
        Bn {
            bin,
            instance,
            target,
            client,
            health: Arc::new(Mutex::new(None)),
        }
    }

    /// Whether this handle talks to the bridge directly. Surfaced so the header can
    /// show the transport actually in use — a silent fallback to CLI spawns would
    /// otherwise look like an unexplained 100x slowdown.
    pub fn is_direct(&self) -> bool {
        self.client.is_some()
    }

    /// Run `op` over the socket, or `None` when there is no client and the caller
    /// must fall back to the CLI. Errors are recorded against `key` in the shared
    /// health cell when `scope` is [`HealthScope::Shared`], preserving the existing
    /// "a failed state read stays visible until that same family succeeds" rule.
    fn call(
        &self,
        op: &str,
        params: serde_json::Value,
        key: &[&str],
        scope: HealthScope,
    ) -> Option<Result<serde_json::Value, String>> {
        let client = self.client.as_ref()?;
        let result = client.request(op, params, self.target.as_deref());
        if scope == HealthScope::Shared {
            match &result {
                Ok(_) => self.record_success(key),
                Err(message) => self.record_failure(key, message.clone()),
            }
        }
        Some(result)
    }

    /// Bridge plugin version, when talking to one directly.
    ///
    /// There is no version negotiation on the wire and no schema version in the
    /// envelope (`DESIGN_BN_INTERFACE.md` §1), so a bn upgrade that reshapes a
    /// result is invisible until a field stops deserializing. Naming the version in
    /// that error turns "the lens broke" into "the lens broke against bridge
    /// 0.21.0" — free, because the registry file we already parsed for the socket
    /// path carries it.
    pub fn bridge_version(&self) -> Option<&str> {
        self.client
            .as_ref()
            .map(|client| client.plugin_version.as_str())
            .filter(|version| !version.is_empty())
    }

    /// Deserialize a bridge `result`, naming the op and the bridge version that
    /// produced an unexpected shape. See [`Self::bridge_version`] for why.
    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        value: serde_json::Value,
        what: &str,
    ) -> Result<T, String> {
        serde_json::from_value(value).map_err(|error| match self.bridge_version() {
            Some(version) => {
                format!("bn {what} returned unexpected JSON (bridge {version}): {error}")
            }
            None => format!("bn {what} returned unexpected JSON: {error}"),
        })
    }

    /// [`Self::call`] for per-item reads: the caller renders the failure itself, so
    /// shared health is neither poisoned nor healed.
    fn call_local(
        &self,
        op: &str,
        params: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        self.call(op, params, &[], HealthScope::Local)
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
        // The `list_targets` op returns a BARE array, not the `{items: [...]}`
        // envelope the CLI wraps it in — so this cannot reuse `TargetListJson`.
        if let Some(client) = &self.client {
            // Listing is instance-scoped: no target selector.
            let result = client.request("list_targets", serde_json::json!({}), None);
            match result {
                Ok(value) => {
                    self.record_success(&["target", "list"]);
                    return self.decode::<Vec<TargetItem>>(value, "target list").map_err(|message| {
                        self.record_failure(&["target", "list"], message.clone());
                        message
                    });
                }
                Err(message) => {
                    self.record_failure(&["target", "list"], message.clone());
                    return Err(message);
                }
            }
        }
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

    /// Run `args` and return stdout *regardless of exit status* — for commands
    /// whose useful payload is printed even when they exit non-zero, e.g. a
    /// failed `--preview` validation whose `--summary` JSON still carries the
    /// `first_error`. Empty when bn can't be started.
    fn run_stdout(&self, args: &[&str]) -> String {
        self.cmd()
            .args(args)
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
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
        // Instance discovery needs no bridge round-trip at all: the registry files
        // under `<cache>/bn/instances/` already carry id, binaries, and start time,
        // and liveness is a connect probe. This removes the last `bn` spawn from
        // instance resolution — which ran up to 3 times on the retry path.
        let live = crate::bnsock::list_instances();
        if !live.is_empty() {
            return live
                .into_iter()
                .map(|client| Instance {
                    instance_id: client.instance_id,
                    binaries: client.binaries,
                    started_at: client.started_at,
                })
                .collect();
        }
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
            let page: FunctionListJson = if let Some(result) = self.call(
                "list_functions",
                // No `limit`: on the wire an absent limit means "no limit"
                // (bridge.py:3153), so the whole inventory arrives in one round
                // trip. The CLI's `_effective_limit` layer does not exist here.
                serde_json::json!({"offset": offset}),
                &["function", "list"],
                HealthScope::Shared,
            ) {
                self.decode(result?, "function list").map_err(|message| {
                    self.record_failure(&["function", "list"], message.clone());
                    message
                })?
            } else {
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
                serde_json::from_str(out.trim()).map_err(|error| {
                    let message = format!("bn function list returned invalid JSON: {error}");
                    self.record_failure(&["function", "list"], message.clone());
                    message
                })?
            };
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
                    size: item.size,
                    basic_block_count: item.basic_block_count,
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
        if let (Some(comments), Some(tags)) = (
            self.call(
                "list_comments",
                serde_json::json!({}),
                &["comment", "list"],
                HealthScope::Shared,
            ),
            self.call(
                "list_tags",
                serde_json::json!({}),
                &["tag", "list"],
                HealthScope::Shared,
            ),
        ) {
            if let Ok(list) = comments.and_then(|v| self.decode::<CommentListJson>(v, "comment list"))
            {
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
            if let Ok(list) = tags.and_then(|v| self.decode::<TagListJson>(v, "tag list")) {
                for t in list.items {
                    let kind = t
                        .type_name
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "tag".into());
                    push_mark(&mut marks, t.address, kind, t.data, t.function);
                }
            }
            return marks;
        }
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
            let page: ImportsJson = if let Some(result) = self.call(
                "imports",
                serde_json::json!({"offset": offset}),
                &["imports"],
                HealthScope::Shared,
            ) {
                self.decode(result?, "imports").map_err(|message| {
                    self.record_failure(&["imports"], message.clone());
                    message
                })?
            } else {
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
                serde_json::from_str(out.trim()).map_err(|error| {
                    let message = format!("bn imports returned invalid JSON: {error}");
                    self.record_failure(&["imports"], message.clone());
                    message
                })?
            };
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

    /// `bn types` -> the full type list (name + kind) for the Types view,
    /// paged with `--offset` until the envelope reports `has_more: false` so a
    /// database past the page size is never silently truncated.
    pub fn types_list(&self) -> Vec<TypeItem> {
        let mut all: Vec<TypeItem> = Vec::new();
        loop {
            let offset = all.len();
            let (mut items, has_more) = if let Some(result) = self.call(
                "types",
                serde_json::json!({"offset": offset}),
                &["types"],
                HealthScope::Shared,
            ) {
                match result.and_then(|value| self.decode::<TypesJson>(value, "types")) {
                    Ok(page) => types_page(page),
                    Err(_) => (Vec::new(), false),
                }
            } else {
                let offset = offset.to_string();
                let out = self.run_out_state(&[
                    "types", "--offset", &offset, "--limit", "5000", "--format", "json",
                ]);
                parse_types_page(&out)
            };
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
            let page: ClassesJson = if let Some(result) = self.call(
                "class_list",
                serde_json::json!({"offset": offset, "no_stl": true, "no_vendor": true}),
                &["class", "list"],
                HealthScope::Shared,
            ) {
                self.decode(result?, "class list").map_err(|message| {
                    self.record_failure(&["class", "list"], message.clone());
                    message
                })?
            } else {
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
                serde_json::from_str(out.trim()).map_err(|error| {
                    let message = format!("bn class list returned invalid JSON: {error}");
                    self.record_failure(&["class", "list"], message.clone());
                    message
                })?
            };
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
            let page: ExportsJson = if let Some(result) = self.call(
                "list_exports",
                serde_json::json!({"offset": offset}),
                &["exports"],
                HealthScope::Shared,
            ) {
                self.decode(result?, "exports").map_err(|message| {
                    self.record_failure(&["exports"], message.clone());
                    message
                })?
            } else {
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
                serde_json::from_str(out.trim()).map_err(|error| {
                    let message = format!("bn exports returned invalid JSON: {error}");
                    self.record_failure(&["exports"], message.clone());
                    message
                })?
            };
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
        if let Some(blocks) = self.read_op::<CfgJson>(
            "cfg",
            serde_json::json!({"identifier": ident, "view": il}),
        ) {
            return blocks.map(|cfg| cfg.blocks).unwrap_or_default();
        }
        let escaped = ident.replace('\\', "\\\\").replace('\'', "\\'");
        let program = CFG_PROGRAM.replace("{IDENT}", &escaped).replace("{IL}", il);
        self.py_json::<CfgJson>(&program)
            .map(|cfg| cfg.blocks)
            .unwrap_or_default()
    }

    /// Call a first-class read-locked op, distinguishing "this bridge is too old to
    /// have the op" from "the op ran and failed".
    ///
    /// `None` means the bridge does not know the op (or there is no socket), so the
    /// caller should run its legacy `py exec` program. `Some(Err)` means the op
    /// exists and genuinely failed — do NOT fall back then, or a real error would be
    /// masked by a second attempt that takes the exclusive lock.
    fn read_op<T: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        params: serde_json::Value,
    ) -> Option<Result<T, String>> {
        let result = self.call_local(op, params)?;
        match result {
            Ok(value) => Some(self.decode(value, op)),
            // The bridge's dispatch answers an unregistered op with exactly this
            // (`bridge.py`: "Unknown operation: <name>"), which is how a lens built
            // against a newer bn keeps working against an older live bridge.
            Err(error) if is_unknown_op(&error) => None,
            Err(error) => Some(Err(error)),
        }
    }

    /// Run a legacy `py exec` program and parse the JSON it printed.
    ///
    /// WARNING: `py_exec` is registered `lock="write"`, so this takes the bridge's
    /// *exclusive* lock and blocks every concurrent read — including the paired
    /// agent's (measured: a 0.7 ms read becomes 401 ms under load). This path now
    /// exists only for bridges predating the read-locked `cfg` / `data_vars` /
    /// `data_symbols` ops; it can be deleted once those have shipped everywhere.
    fn py_json<T: serde::de::DeserializeOwned>(&self, program: &str) -> Option<T> {
        if let Some(result) = self.call_local("py_exec", serde_json::json!({"script": program})) {
            let envelope: PyEnvelope = serde_json::from_value(result.ok()?).ok()?;
            return serde_json::from_str::<T>(&envelope.stdout).ok();
        }
        let out = self.run_out(&["py", "exec", "--format", "json", "--code", program]);
        serde_json::from_str::<PyEnvelope>(&out)
            .ok()
            .and_then(|envelope| serde_json::from_str::<T>(&envelope.stdout).ok())
    }

    /// BN's typed data variables in the half-open address window `[lo, hi)`
    /// (`0x…` strings), ascending — the backing for the structured data-section
    /// view. Empty on any failure (bad window, py unavailable, malformed output),
    /// so the caller can fall back to a raw byte dump.
    pub fn data_vars(&self, lo: &str, hi: &str) -> Vec<DataVar> {
        if let Some(vars) = self.read_op::<DataMapJson>(
            "data_vars",
            serde_json::json!({"start": lo, "end": hi}),
        ) {
            return vars.map(|d| d.vars).unwrap_or_default();
        }
        let program = DATA_MAP_PROGRAM.replace("{LO}", lo).replace("{HI}", hi);
        self.py_json::<DataMapJson>(&program)
            .map(|d| d.vars)
            .unwrap_or_default()
    }

    /// `(address, name)` for every named data symbol — including internal ones
    /// the exports list omits — so a renamed data global stays interactive
    /// (hotspots, peek, xref). Via `bn py exec`; empty on failure, in which case
    /// the lens degrades to exports + `data_<hex>` recognition.
    pub fn data_symbols(&self) -> Vec<(String, String)> {
        if let Some(syms) = self.read_op::<DataSymsJson>("data_symbols", serde_json::json!({})) {
            return syms
                .map(|d| d.syms.into_iter().map(|s| (s.a, s.n)).collect())
                .unwrap_or_default();
        }
        self.py_json::<DataSymsJson>(DATA_SYMBOLS_PROGRAM)
            .map(|d| d.syms.into_iter().map(|s| (s.a, s.n)).collect())
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
        if let Some(result) =
            self.call_local("decompile", serde_json::json!({"identifier": name}))
        {
            return match result {
                Ok(value) => match decompile_render(&value) {
                    text if text.trim().is_empty() => "(no output)".into(),
                    text => text,
                },
                Err(error) => format!("✗ {error}"),
            };
        }
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
        if let Some(result) = self.call_local(
            "decompile",
            serde_json::json!({"identifier": name, "addresses": true}),
        ) {
            return match result {
                Ok(value) => text_field(&value),
                Err(error) => format!("✗ {error}"),
            };
        }
        self.run_out(&["decompile", name, "--addresses"])
    }

    /// Decompile `id` — a function name or any *interior* address (bn resolves
    /// it to the containing function) — as JSON, returning
    /// `(function name, entry address, address-prefixed text)`. The JSON gives
    /// us the resolved identity directly instead of scraping the text header.
    pub fn decompile_json(&self, id: &str) -> Option<(String, String, String)> {
        if let Some(result) = self.call_local(
            "decompile",
            serde_json::json!({"identifier": id, "addresses": true}),
        ) {
            let parsed: DecompiledFn = serde_json::from_value(result.ok()?).ok()?;
            return (!parsed.text.is_empty())
                .then(|| (parsed.function.name, parsed.function.address, parsed.text));
        }
        let out = self.run_out(&["decompile", id, "--addresses", "--format", "json"]);
        let parsed: DecompiledFn = serde_json::from_str(&out).ok()?;
        if parsed.text.is_empty() {
            None
        } else {
            Some((parsed.function.name, parsed.function.address, parsed.text))
        }
    }

    /// One `decompile --addresses` read carrying *everything* the CLI's text mode
    /// reconstructs around the body — the interior-resolution note and the
    /// `warnings[]` entries — alongside the address-prefixed pseudo-C.
    ///
    /// This exists so a caller can derive both the rendered lines and the
    /// per-line address column from a **single** backend read: the viewer used to
    /// issue one plain `decompile` for the text and a second `--addresses`
    /// decompile for the addresses, and reconciled the two by index. On a live
    /// instance a concurrent rename/retype between the two reads shifted the line
    /// set and every address readout with it (issue #18).
    pub fn decompile_read(&self, id: &str) -> Option<DecompileRead> {
        let value = match self.call_local(
            "decompile",
            serde_json::json!({"identifier": id, "addresses": true}),
        ) {
            Some(result) => result.ok()?,
            None => serde_json::from_str(&self.run_out(&[
                "decompile",
                id,
                "--addresses",
                "--format",
                "json",
            ]))
            .ok()?,
        };
        let parsed: DecompiledFn = serde_json::from_value(value.clone()).ok()?;
        if parsed.text.is_empty() {
            return None;
        }
        let note = resolution_note(&value);
        Some(DecompileRead {
            name: parsed.function.name,
            entry: parsed.function.address,
            text: parsed.text,
            note: (!note.is_empty()).then(|| note.trim_end().to_string()),
            warnings: warning_lines(&value),
        })
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
        if let Some(result) = self.call(
            "target_info",
            serde_json::json!({}),
            &["target", "info"],
            HealthScope::Shared,
        ) {
            let info: TargetInfoJson = self.decode(result?, "target info").map_err(|message| {
                self.record_failure(&["target", "info"], message.clone());
                message
            })?;
            return Ok(TargetInfo {
                arch: info.arch,
                analysis_state: AnalysisState::from_raw(&info.analysis_state),
            });
        }
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
        if let Some(result) =
            self.call("sections", serde_json::json!({}), &["sections"], HealthScope::Shared)
        {
            return result.map(|value| render_sections(&value));
        }
        // NOTE (audit N2): this CLI fallback has no `--out`, so bn's paged default
        // caps it at 100 rows (cli.py:1286-1296) and a target with more sections
        // silently loses ranges. The socket path above has no such cap — absent
        // `limit` means unlimited on the wire — so the bug only survives in the
        // fallback. Left as-is rather than fixed twice; the fallback is transitional.
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
        if let Some(result) =
            self.call("strings", serde_json::json!({}), &["strings"], HealthScope::Shared)
        {
            let Ok(value) = result else {
                return Vec::new();
            };
            let items = value
                .get("items")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            return items
                .into_iter()
                .filter_map(|item| {
                    let addr = item.get("address")?.as_str()?.to_string();
                    let raw = item.get("value")?.as_str()?;
                    // Re-escape to the CLI's rendering: the content→address map is
                    // keyed on what the decompiler prints between quotes, not on
                    // the raw bytes. See `json_escape_ascii`.
                    Some((json_escape_ascii(raw), addr))
                })
                .collect();
        }
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
        if let Some(result) =
            self.call_local("list_locals", serde_json::json!({"identifier": func}))
        {
            return result
                .ok()
                .and_then(|value| serde_json::from_value::<LocalListJson>(value).ok())
                .map(|listing| listing.locals)
                .unwrap_or_default();
        }
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

    /// Retype a local (`bn local retype`). Live in-memory; not persisted until a
    /// deliberate `bn save`. Returns whether it committed.
    pub fn local_retype(&self, func: &str, var: &str, new_type: &str) -> bool {
        let out = self.run(&["local", "retype", func, var, new_type, "--summary"]);
        mutation_ok(&out)
    }

    /// Validate a retype *without* committing (`--preview`: apply, diff, revert).
    /// `Ok(())` if the type parses and applies cleanly, else the parser error —
    /// so the composer can confirm a change before it touches the instance.
    pub fn local_retype_check(&self, func: &str, var: &str, new_type: &str) -> Result<(), String> {
        // stdout carries the `--summary` JSON (with `first_error`) even when a
        // bad type makes bn exit non-zero, so read it regardless of exit status.
        let out = self.run_stdout(&[
            "local",
            "retype",
            func,
            var,
            new_type,
            "--preview",
            "--summary",
        ]);
        for line in out.lines() {
            if let Ok(summary) = serde_json::from_str::<MutationSummaryJson>(line.trim()) {
                return if summary.success {
                    Ok(())
                } else {
                    Err(clean_err(
                        &summary
                            .first_error
                            .unwrap_or_else(|| "type rejected".into()),
                    ))
                };
            }
        }
        Err("no response from bn".into())
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

    /// The comment currently at `addr` (`comment get`), for edit-in-place.
    /// `None` when there is none (or on failure).
    pub fn comment_get_addr(&self, addr: &str) -> Option<String> {
        let out = self.run_stdout(&["comment", "get", addr, "--format", "json"]);
        let value: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
        if value
            .get("has_comment")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            value
                .get("comment")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        } else {
            None
        }
    }

    /// A function's existing annotation: BN's separate `function_doc`, plus any
    /// comment sitting at the entry address (which BN *also* renders atop the
    /// function — e.g. one added with `bn comment set <entry>`). Lets `;` edit
    /// whichever note is actually shown, in place.
    pub fn comment_get_func(&self, func: &str) -> FuncComment {
        let out = self.run_stdout(&["comment", "get", "--function", func, "--format", "json"]);
        let Ok(value) = serde_json::from_str::<serde_json::Value>(out.trim()) else {
            return FuncComment::default();
        };
        let doc = value
            .get("function_doc")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let entry_addr = value
            .get("address")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let entry_comment = value
            .get("comments")
            .and_then(serde_json::Value::as_array)
            .and_then(|items| {
                items.iter().find_map(|item| {
                    let addr = item.get("address").and_then(serde_json::Value::as_str)?;
                    (addr == entry_addr)
                        .then(|| item.get("comment").and_then(serde_json::Value::as_str))
                        .flatten()
                        .map(str::to_string)
                })
            });
        FuncComment {
            doc,
            entry_addr,
            entry_comment,
        }
    }

    /// Set a function's documentation comment (shown atop the function).
    pub fn comment_set_func(&self, func: &str, text: &str) -> bool {
        let out = self.run(&["comment", "set", "--function", func, text, "--summary"]);
        mutation_ok(&out)
    }

    /// Delete the comment at `addr` (clearing a comment edited down to empty).
    pub fn comment_delete_addr(&self, addr: &str) -> bool {
        let out = self.run(&["comment", "delete", addr, "--summary"]);
        mutation_ok(&out)
    }

    /// Delete a function's documentation comment.
    pub fn comment_delete_func(&self, func: &str) -> bool {
        let out = self.run(&["comment", "delete", "--function", func, "--summary"]);
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

    /// Like [`Self::read`] but via `--out`, so a large window (the linear data
    /// view can request several KB) doesn't trip bn's stdout spill envelope.
    /// Empty on failure.
    pub fn read_dump(&self, addr: &str, length: usize) -> String {
        let len = length.to_string();
        let out = self.run_out(&["read", addr, "--length", &len]);
        if out.trim_start().starts_with('✗') {
            String::new()
        } else {
            out
        }
    }
}

/// The socket client for `instance`, or — when none was named — the single live
/// instance that has a binary open.
///
/// Ambiguity deliberately resolves to `None` rather than guessing: with several
/// bridges live, picking one would silently drive a different binary than the user
/// asked for. `None` falls back to the CLI, whose own resolution ladder (env →
/// `.bn-<id>` marker → single live) is the documented behavior.
fn resolve_client(instance: Option<&str>) -> Option<crate::bnsock::Client> {
    match instance {
        Some(id) if !id.is_empty() && id != "(default)" => crate::bnsock::open(id),
        _ => {
            let mut live: Vec<crate::bnsock::Client> = crate::bnsock::list_instances()
                .into_iter()
                .filter(|client| !client.binaries.is_empty())
                .collect();
            (live.len() == 1).then(|| live.remove(0))
        }
    }
}

/// Whether a bridge error means the op is not registered on THAT bridge, rather
/// than that the op ran and failed.
///
/// `bridge.dispatch` answers an unregistered op with exactly `Unknown operation:
/// <name>`, so this is what lets a lens built against a newer bn keep working
/// against an older live bridge — falling back to the legacy `py exec` program
/// instead of losing the view entirely.
fn is_unknown_op(error: &str) -> bool {
    error.starts_with("Unknown operation")
}

/// The `text` field of a `decompile`/`il`/`disasm` result.
///
/// These ops return the *same* text the CLI prints — verified byte-identical for
/// `decompile` apart from a trailing newline the CLI appends, which `.lines()`
/// discards either way (`DESIGN_BN_INTERFACE.md` §7 spike).
fn text_field(value: &serde_json::Value) -> String {
    value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Render a socket `decompile` result the way the CLI's text mode did
/// (`function.py:246-257`): a leading resolution note, the pseudo-C, then any
/// warnings.
///
/// Reading only `text` off the socket silently dropped both — the interior-address
/// disclosure and every `warnings[]` entry: the #446 thunk/veneer "this is a
/// PLT/GOT trampoline, not self-recursion" note, the analysis-stub warning, and
/// the pseudo-C→wrapped-HLIL degradation notice (`read_decompile.py:144-162`).
/// Those are load-bearing on a live target, so the socket path must reconstruct
/// them rather than leave the reader with unexplained-looking output.
fn decompile_render(value: &serde_json::Value) -> String {
    let mut body = text_field(value);
    let warnings = warning_lines(value);
    if !warnings.is_empty() {
        body = format!("{body}\n\n{}", warnings.join("\n"));
    }
    format!("{}{body}", resolution_note(value))
}

/// The `warnings[]` entries of a `decompile` result as the `warning: …` lines the
/// CLI's text mode appends below the body, in order. Empty when there are none.
fn warning_lines(value: &serde_json::Value) -> Vec<String> {
    value
        .get("warnings")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|warning| format!("warning: {warning}"))
                .collect()
        })
        .unwrap_or_default()
}

/// The CLI's `_resolution_note` (`formatters.py:124-143`): a `// bn: …` line when a
/// function-scoped read resolved an interior address to its containing function.
/// Empty when the result carries no `resolved_from` (an exact start or a name),
/// which is the common case. `requested_address` and `offset` are already-rendered
/// strings from the bridge (`seam.py:430`).
fn resolution_note(value: &serde_json::Value) -> String {
    let Some(resolved) = value.get("resolved_from").filter(|v| v.is_object()) else {
        return String::new();
    };
    let field = |parent: Option<&serde_json::Value>, key: &str| {
        parent
            .and_then(|v| v.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    let function = value.get("function");
    format!(
        "// bn: {} is inside {} @ {} ({}); showing the containing function\n",
        field(Some(resolved), "requested_address"),
        field(function, "name"),
        field(function, "address"),
        field(Some(resolved), "offset"),
    )
}

/// Re-encode a string the way Python's `json.dumps(s, ensure_ascii=True)` does,
/// **without** the surrounding quotes.
///
/// This is load-bearing, not cosmetic. `Ctx::strings` keys a content→address map on
/// the text bn's CLI printed between quotes (`formatters.py:2601` is literally
/// `json.dumps(value, ensure_ascii=True)`), and `hotspots.rs` looks a decompiled
/// string literal up in that map. Reading `value` raw off the socket would change
/// the key for every string containing a non-ASCII byte, silently regressing string
/// peek/xref to "couldn't resolve string" — the exact trap recorded as L4 in
/// `DESIGN_BN_INTERFACE.md`.
///
/// Python escapes everything outside printable ASCII (`[^\ -~]`), preferring the
/// short forms for the five whitespace controls, and emits surrogate pairs for
/// astral codepoints.
fn json_escape_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // Printable ASCII passes through untouched; everything else — other
            // controls, DEL, and all non-ASCII — becomes \uXXXX.
            ' '..='~' => out.push(ch),
            _ => {
                let cp = ch as u32;
                if cp > 0xFFFF {
                    // Astral plane: Python emits a UTF-16 surrogate pair.
                    let v = cp - 0x1_0000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    ));
                } else {
                    out.push_str(&format!("\\u{cp:04x}"));
                }
            }
        }
    }
    out
}

/// Render the `sections` envelope the way the CLI does
/// (`formatters.py:2630-2672`), so the `s` popup keeps its current look and
/// `ctx::parse_section_ranges` keeps parsing the same token order.
///
/// The `w+x:` verdict line is part of the contract, not decoration — the popup
/// title advertises it, and it is the view's one cheap security signal. It is
/// reconstructed here from `wx_verdict` / `writable_executable_items`.
fn render_sections(value: &serde_json::Value) -> Vec<String> {
    let mut lines = Vec::new();
    match value.get("wx_verdict").and_then(serde_json::Value::as_str) {
        Some("wx_sections_present") => {
            let names: Vec<&str> = value
                .get("writable_executable_items")
                .and_then(serde_json::Value::as_array)
                .map(|items| items.iter().filter_map(serde_json::Value::as_str).collect())
                .unwrap_or_default();
            lines.push(format!(
                "w+x: {} section(s): {}",
                names.len(),
                names.join(", ")
            ));
        }
        Some("no_wx_sections_observed") => lines.push("w+x: none observed".into()),
        Some(_) => lines.push(
            "w+x: unknown -- section metadata is insufficient (mapped/raw view with no \
             segment permissions); NOT an all-clear"
                .into(),
        ),
        None => {}
    }
    let items = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    // The CLI's `_render_sections_rows` prints `none` for an empty set (below any
    // w+x line); mirror it so the popup never renders as a bare verdict with no body.
    if items.is_empty() {
        lines.push("none".into());
        return lines;
    }
    for item in items {
        let get = |key: &str| {
            item.get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
                .to_string()
        };
        let flag = |key: &str, on: char| {
            if item.get(key).and_then(serde_json::Value::as_bool) == Some(true) {
                on
            } else {
                '-'
            }
        };
        let perms = if item.get("readable").is_some() {
            format!(
                "{}{}{}",
                flag("readable", 'r'),
                flag("writable", 'w'),
                flag("executable", 'x')
            )
        } else {
            String::new()
        };
        let length = item
            .get("length")
            .map(|value| match value.as_i64() {
                Some(n) => n.to_string(),
                None => "?".into(),
            })
            .unwrap_or_else(|| "?".into());
        let semantics = item
            .get("semantics")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let name = item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<unknown>");
        let line = format!(
            "{}-{}  {length:>8}  {perms:>3}  {semantics:<20}  {name}",
            get("start"),
            get("end")
        );
        lines.push(line.trim_end().to_string());
    }
    lines
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
        decompile_render, is_unknown_op, json_escape_ascii, parse_local_list_json,
        parse_types_page, push_mark, recovered_class, render_sections, text_field, AnalysisState,
        Bn, ClassItemJson, CommentListJson, TagListJson,
    };

    #[test]
    fn string_keys_match_pythons_ensure_ascii_rendering() {
        // The content->address map is keyed on what bn's CLI printed between
        // quotes, i.e. `json.dumps(value, ensure_ascii=True)`. Reading `value` raw
        // off the socket would silently change the key for any non-ASCII string
        // and regress string peek/xref. These expectations are Python's output.
        assert_eq!(json_escape_ascii("plain"), "plain");
        assert_eq!(json_escape_ascii("say \"hi\""), "say \\\"hi\\\"");
        assert_eq!(json_escape_ascii("a\\b"), "a\\\\b");
        assert_eq!(json_escape_ascii("line\nnext"), "line\\nnext");
        assert_eq!(json_escape_ascii("tab\there"), "tab\\there");
        // Non-ASCII must escape, not pass through — this is the whole point.
        assert_eq!(json_escape_ascii("café"), "caf\\u00e9");
        assert_eq!(json_escape_ascii("→"), "\\u2192");
        // DEL and other controls are outside Python's printable range.
        assert_eq!(json_escape_ascii("\u{7f}"), "\\u007f");
        assert_eq!(json_escape_ascii("\u{1}"), "\\u0001");
        // Astral codepoints become a UTF-16 surrogate pair, as Python emits.
        assert_eq!(json_escape_ascii("\u{1F600}"), "\\ud83d\\ude00");
        // Space and tilde are the inclusive bounds of the pass-through range.
        assert_eq!(json_escape_ascii(" ~"), " ~");
    }

    #[test]
    fn sections_render_keeps_the_wx_verdict_and_token_order() {
        // `ctx::parse_section_ranges` splits on whitespace and reads the range
        // first and the name last, so the rendering contract is token ORDER, and
        // the popup's advertised security signal is the leading w+x line.
        let value = serde_json::json!({
            "wx_verdict": "no_wx_sections_observed",
            "items": [{
                "start": "0x400200", "end": "0x40021b", "length": 27,
                "readable": true, "writable": false, "executable": true,
                "semantics": "ReadOnlyData", "name": ".interp"
            }]
        });
        let lines = render_sections(&value);
        assert_eq!(lines[0], "w+x: none observed");
        let cols: Vec<&str> = lines[1].split_whitespace().collect();
        assert_eq!(cols[0], "0x400200-0x40021b");
        assert_eq!(cols[1], "27");
        assert_eq!(cols[2], "r-x");
        assert_eq!(cols[3], "ReadOnlyData");
        assert_eq!(cols.last().copied(), Some(".interp"));
    }

    #[test]
    fn sections_render_names_wx_sections_rather_than_claiming_none() {
        let value = serde_json::json!({
            "wx_verdict": "wx_sections_present",
            "writable_executable_items": [".text", ".data"],
            "items": []
        });
        assert_eq!(
            render_sections(&value)[0],
            "w+x: 2 section(s): .text, .data"
        );
    }

    #[test]
    fn sections_render_never_reads_unknown_metadata_as_an_all_clear() {
        let value = serde_json::json!({
            "wx_verdict": "unknown_insufficient_metadata", "items": []
        });
        let first = render_sections(&value)[0].clone();
        assert!(first.starts_with("w+x: unknown"));
        assert!(first.contains("NOT an all-clear"));
    }

    #[test]
    fn only_an_unregistered_op_triggers_the_legacy_fallback() {
        // Falling back re-runs the work through write-locked py_exec, which stalls
        // the paired agent. That is acceptable to keep an OLD bridge working, but
        // must never happen for a real failure on a bridge that HAS the op —
        // otherwise a genuine error is masked and the lock is taken for nothing.
        assert!(is_unknown_op("Unknown operation: cfg"));
        assert!(!is_unknown_op("Function not found: zzz. Did you mean: open"));
        assert!(!is_unknown_op("internal error: KeyError: 'start'"));
        assert!(!is_unknown_op(""));
    }

    #[test]
    fn text_field_is_empty_rather_than_panicking_on_an_odd_payload() {
        assert_eq!(text_field(&serde_json::json!({"text": "body"})), "body");
        assert_eq!(text_field(&serde_json::json!({})), "");
        assert_eq!(text_field(&serde_json::json!({"text": 7})), "");
    }

    #[test]
    fn decompile_render_appends_the_warnings_the_cli_text_mode_showed() {
        // A #446 thunk/veneer decompile: the socket `text` reads like an infinite
        // self-recursion, and the warning is the only thing that says otherwise.
        // Dropping it (reading `text` alone) is the regression this guards.
        let value = serde_json::json!({
            "text": "int32_t sub_401000()\n{\n    return sub_401000();\n}",
            "warnings": [
                "thunk/veneer -> memcpy @ 0x402000: this is a PLT/GOT trampoline \
                 (a jump to the real body), not a self-recursive function.",
            ],
        });
        let rendered = decompile_render(&value);
        assert!(rendered.starts_with("int32_t sub_401000()"));
        assert!(rendered.contains("\n\nwarning: thunk/veneer -> memcpy @ 0x402000:"));
    }

    #[test]
    fn decompile_render_prefixes_the_interior_address_resolution_note() {
        // A goto/xref bounce to a mid-function address resolves to the container;
        // the leading note is what tells the reader the entry differs on purpose.
        let value = serde_json::json!({
            "text": "void handler()\n{\n}",
            "function": {"name": "handler", "address": "0x401000"},
            "resolved_from": {"requested_address": "0x401014", "offset": "+0x14"},
        });
        let rendered = decompile_render(&value);
        assert_eq!(
            rendered.lines().next(),
            Some("// bn: 0x401014 is inside handler @ 0x401000 (+0x14); showing the containing function")
        );
    }

    #[test]
    fn decompile_render_is_bare_text_when_there_is_nothing_to_annotate() {
        // The common case — an exact-name decompile with no warnings — must be
        // byte-identical to the plain `text`, no stray note or blank lines.
        let value = serde_json::json!({"text": "void f()\n{\n}"});
        assert_eq!(decompile_render(&value), "void f()\n{\n}");
    }

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
