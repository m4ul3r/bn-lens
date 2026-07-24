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
/// three cases, so the lens degrades to its previous behavior instead of failing
/// outright:
///
/// 1. no live registry could be read (an unreadable cache dir, or a bridge that
///    registered after we resolved);
/// 2. the resolution was ambiguous — several live bridges and no named instance
///    (see `resolve_client`);
/// 3. the live bridge is older than the op we want (`read_op` / `mutation_op`
///    detect that and only then re-run over the CLI).
///
/// One read is deliberately CLI-only regardless of transport, and this list is the
/// contract — do not read "whenever a socket exists" as "always": [`Bn::class_show`]
/// (the `class_show` op exists, but its text form is a ~60-line renderer for a cold
/// popup, so replicating it would trade a real divergence risk for ~127 ms on a
/// keypress that already opens a modal). [`Bn::cfg`], [`Bn::data_vars`] and
/// [`Bn::data_symbols`] name read-locked ops that no shipped bridge registers yet,
/// so in practice they still run their legacy `py exec` program.
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
    // `#[serde(default)]` for the reason `TagItemJson` documents below: without it
    // ONE element missing `name` aborts the whole `Vec`, and `local_list`
    // `unwrap_or_default()`s that error into an empty locals list — no `Local`
    // hotspots, `n` on a variable silently renaming the *function* instead, `S`
    // claiming no stack variables were recovered, and nothing for the retype
    // composer to target. `recovered_locals` drops the nameless element instead.
    #[serde(default)]
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

// Both fields carry `#[serde(default)]` for the same reason as `LocalVariable.name`:
// one element short an address or a name would otherwise abort the whole `Vec`, and
// `data_symbols` flattens that to an empty map — renamed globals stop being hotspots,
// so `w`/`b` skip them and `p`/`x` can't target them, which is precisely the
// regression this read exists to prevent (`ctx.rs`). `recovered_data_syms` drops the
// incomplete element instead of the list.
#[derive(Deserialize)]
struct DataSym {
    #[serde(default)]
    a: String,
    #[serde(default)]
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

/// The type layouts a `types declare --preview` would define. Shared by the socket
/// and CLI paths so both read the same field off the same payload.
fn preview_layouts(payload: PreviewPayload) -> Vec<TypeLayout> {
    payload
        .affected_types
        .into_iter()
        .map(|affected| TypeLayout {
            name: affected.name,
            layout: affected.after_layout,
        })
        .collect()
}

/// The first parser error a rejected `types declare --preview` reported.
fn preview_error(payload: PreviewPayload) -> Option<String> {
    payload.results.into_iter().find_map(|result| result.message)
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

/// 8 bytes of unguessable name material for the capture directory, so the path an
/// attacker would have to pre-create (or symlink) cannot be derived from anything
/// they can observe. The clock is only a fallback for a sandbox with no
/// `/dev/urandom`; it still yields a fresh name per process.
fn capture_nonce() -> u64 {
    use std::io::Read;
    let mut bytes = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut bytes))
        .is_ok()
    {
        return u64::from_le_bytes(bytes);
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0)
}

/// The private per-process directory holding `--out` captures, or `None` if one
/// could not be created.
///
/// A capture holds decompiled output of the target under audit — exactly the class
/// this repo's disclosure policy covers — and the old scheme wrote it straight into
/// the shared temp dir under a name derivable from `(pid, call ordinal)`, created by
/// the *child* with no `O_EXCL`. A local attacker could pre-create or symlink that
/// path and either read the capture or redirect the write.
///
/// Two properties close that off, and both matter:
/// * `mode(0o700)` — nobody else can open a file inside, whatever its name;
/// * `create` is **non-recursive**, so it fails with `AlreadyExists` rather than
///   adopting a path that already exists. A success therefore *proves* this process
///   made the directory and no one else holds a descriptor on it.
///
/// `TMPDIR` is still honoured, because `env::temp_dir()` reads it.
fn capture_dir() -> Option<&'static PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        use std::os::unix::fs::DirBuilderExt;
        let base = std::env::temp_dir();
        let pid = std::process::id();
        // Retry on a fresh nonce: the only realistic loser here is a collision, and
        // re-deriving the name is cheaper than failing the read.
        (0..8).find_map(|_| {
            let candidate = base.join(format!("bn-lens-{pid}-{:016x}", capture_nonce()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&candidate)
                .ok()
                .map(|()| candidate)
            })
    })
    .as_ref()
}

/// A unique temp path for a `--out` capture, so concurrent/sequential captures never
/// share a file (see [`Bn::run_out`]).
///
/// Normally `<capture dir>/<seq>.out` inside the 0700 directory above. If that
/// directory could not be created at all we fall back to the shared temp dir, but
/// with the nonce moved into the *file* name — an unguessable name still denies the
/// pre-creation/symlink race, which is the attack; it just can't also deny a reader
/// who already knows the name.
fn unique_tmp() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    match capture_dir() {
        Some(dir) => dir.join(format!("{seq}.out")),
        None => std::env::temp_dir().join(format!(
            "bn-lens-{}-{:016x}-{seq}.out",
            std::process::id(),
            capture_nonce()
        )),
    }
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

/// Locals from one `list_locals` / `local list` payload, dropping any element that
/// came back without a name.
///
/// A nameless local is unusable — every consumer keys on the name or the stable id —
/// but it must cost only *itself*. Before `LocalVariable.name` carried
/// `#[serde(default)]` it cost the entire list; keeping the filter here means the
/// default can never quietly promote a junk element into a rename target.
fn recovered_locals(listing: LocalListJson) -> Vec<LocalVariable> {
    listing
        .locals
        .into_iter()
        .filter(|local| !local.name.is_empty())
        .collect()
}

fn parse_local_list_json(text: &str) -> Vec<LocalVariable> {
    serde_json::from_str::<LocalListJson>(text)
        .map(recovered_locals)
        .unwrap_or_default()
}

/// `(address, name)` pairs from one `data_symbols` payload, dropping any element
/// missing either half — see the note on [`DataSym`].
fn recovered_data_syms(payload: DataSymsJson) -> Vec<(String, String)> {
    payload
        .syms
        .into_iter()
        .filter(|sym| !sym.a.is_empty() && !sym.n.is_empty())
        .map(|sym| (sym.a, sym.n))
        .collect()
}

/// Build a `bn` subcommand argv with an explicit `--` end-of-options separator.
///
/// `verb` is the trusted subcommand plus any flags; `positionals` are **untrusted** —
/// anything derived from the binary (function/type/class names, addresses) *and*
/// anything the user typed (comment bodies, tag notes, type declarations). Without
/// the separator `bn`'s argparse reads a dash-leading operand as a flag: `bn comment
/// set 0x401000 -wip` dies with "the following arguments are required: text" and
/// [`mutation_ok`] reports a bare failure with no hint why. A comment opening with a
/// dash is ordinary prose, so this is a correctness bug long before it is a security
/// one — and it is only correctness: there is no shell (`Command::new` + `args`).
///
/// Flags therefore go in `verb`, **before** the separator: after `--`, argparse would
/// take `--summary` as a positional too.
///
/// A dash-leading *flag value* is a different argparse failure ("expected one
/// argument") that `--` cannot fix; use [`flag_eq`] for those.
fn cli_argv<'a>(verb: &[&'a str], positionals: &[&'a str]) -> Vec<&'a str> {
    let mut argv = verb.to_vec();
    if !positionals.is_empty() {
        argv.push("--");
        argv.extend_from_slice(positionals);
    }
    argv
}

/// `--flag=value`, the only form argparse accepts for a value that starts with a
/// dash (a tag note like `-> see parse_hdr` is rejected as `--data <note>`).
fn flag_eq(flag: &str, value: &str) -> String {
    format!("{flag}={value}")
}

/// Splice bn's own `--out <path>` capture flag into `args` **ahead of** any `--`
/// separator [`cli_argv`] inserted.
///
/// Appending it is what the code did before there was a separator, and doing so now
/// would be silently wrong: argparse stops parsing options at `--`, so bn's own
/// `--out` and the path would arrive as two stray positionals and every large read
/// would fail. Nothing to splice past when there is no separator — the flag lands at
/// the end, exactly as before.
fn with_capture(args: &[&str], out: &str) -> Vec<String> {
    let at = args.iter().position(|arg| *arg == "--").unwrap_or(args.len());
    let mut argv: Vec<String> = args[..at].iter().map(|arg| (*arg).to_string()).collect();
    argv.push("--out".into());
    argv.push(out.to_string());
    argv.extend(args[at..].iter().map(|arg| (*arg).to_string()));
    argv
}

/// Statuses `bn`'s CLI counts as a failed mutation row (`formatters.py:14`).
const FAILED_MUTATION_STATUSES: [&str; 5] = [
    "unsupported",
    "verification_failed",
    "invalid_request",
    "rollback_failed",
    "internal_error",
];

/// The `--summary` fields the lens consumes, recomputed from a **raw** socket
/// mutation result.
///
/// `--summary` is a pure CLI-side collapse (`formatters.py::_mutation_summary`) — the
/// bridge always returns the full result — so a write that goes over the socket has
/// to derive the same verdict here or it would read a different outcome than the same
/// write through the CLI.
///
/// `success` is deliberately verification-aware, matching the CLI exactly: the
/// bridge's own `success` **and** no row in [`FAILED_MUTATION_STATUSES`]. Reading
/// `success` alone would report a rolled-back, verification-failed write as a
/// successful one — the lens would then retext a rename that never landed.
#[derive(Default)]
struct MutationOutcome {
    success: bool,
    /// Rows that changed state *and* verified — the CLI's `changed_count`.
    changed_count: u64,
    first_error: Option<String>,
}

fn mutation_outcome(value: &serde_json::Value) -> MutationOutcome {
    let no_rows = Vec::new();
    let rows = value
        .get("results")
        .and_then(serde_json::Value::as_array)
        .unwrap_or(&no_rows);
    let status = |row: &serde_json::Value| {
        row.get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let failed = rows
        .iter()
        .find(|row| FAILED_MUTATION_STATUSES.contains(&status(row).as_str()));
    // The CLI defaults an absent `success` to true; mirror that rather than reading a
    // shape change as a failed write.
    let reported = value
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let success = reported && failed.is_none();
    let changed_count = rows.iter().filter(|row| status(row) == "verified").count() as u64;
    let first_error = (!success).then(|| {
        failed
            .and_then(|row| {
                row.get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some(status(row)).filter(|s| !s.is_empty()))
            })
            // A failure can carry its only explanation top-level — e.g. a revert that
            // failed *after* every op verified, so no row is in the failed set.
            .or_else(|| {
                value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "mutation failed".into())
    });
    MutationOutcome {
        success,
        changed_count,
        first_error,
    }
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
            .args(with_capture(args, &tmp))
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
        let out = match self.op_raw("type_info", serde_json::json!({"type_name": name})) {
            Some(Ok(value)) => render_type_info(&value).unwrap_or_default(),
            Some(Err(error)) => format!("✗ {error}"),
            None => self.run_out(&cli_argv(&["types", "show"], &[name])),
        };
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
        if let Some(result) = self.op_raw(
            "types_declare",
            serde_json::json!({"declaration": decl, "preview": true}),
        ) {
            // `types_declare` is write-locked even under `preview: true` — the bridge
            // applies, diffs, then reverts — so this deliberately goes through the
            // same op, not a read.
            return match result {
                Err(error) => TypeCheck::Err(clean_err(&error)),
                Ok(value) => {
                    let outcome = mutation_outcome(&value);
                    // `ok` is added by the CLI, not the bridge (`_add_mutation_ok`), so
                    // recompute it rather than reading a field that is never present.
                    match serde_json::from_value::<PreviewPayload>(value) {
                        Ok(payload) if outcome.success => TypeCheck::Ok(preview_layouts(payload)),
                        Ok(payload) => TypeCheck::Err(clean_err(
                            &preview_error(payload)
                                .or(outcome.first_error)
                                .unwrap_or_else(|| "declaration rejected".into()),
                        )),
                        Err(_) => TypeCheck::Err("no response from bn".into()),
                    }
                }
            };
        }
        let out = self.run_out(&cli_argv(
            &["types", "declare", "--preview", "--format", "json"],
            &[decl],
        ));
        match serde_json::from_str::<PreviewPayload>(out.trim()) {
            Ok(payload) if payload.ok => TypeCheck::Ok(preview_layouts(payload)),
            Ok(payload) => TypeCheck::Err(clean_err(
                &preview_error(payload).unwrap_or_else(|| "declaration rejected".into()),
            )),
            Err(_) => TypeCheck::Err("no response from bn".into()),
        }
    }

    /// Commit a C declaration as user types (`types declare`). Live in the bn
    /// instance, like every other write — not persisted until an explicit
    /// `bn save`. Returns the count of defined types, or the parser error.
    pub fn type_declare(&self, decl: &str) -> Result<u64, String> {
        if let Some(result) =
            self.mutation_op("types_declare", serde_json::json!({"declaration": decl}))
        {
            return match result {
                Ok(outcome) if outcome.success => Ok(outcome.changed_count),
                Ok(outcome) => Err(outcome
                    .first_error
                    .map(|error| clean_err(&error))
                    .unwrap_or_else(|| "declaration failed".into())),
                Err(error) => Err(clean_err(&error)),
            };
        }
        let out = self.run_out(&cli_argv(
            &["types", "declare", "--summary", "--format", "json"],
            &[decl],
        ));
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

    /// `bn class show <name>` -> the rendered class evidence.
    ///
    /// Deliberately CLI-only: the `class_show` op exists, but its text form is a
    /// ~60-line renderer (vtable slots, secondary vtables, construction sites) feeding
    /// a cold popup, so re-implementing it would trade a real divergence risk against
    /// ~127 ms on a keypress that already opens a modal. See the note on [`Bn`].
    pub fn class_show(&self, name: &str) -> Vec<String> {
        match self.run_out_checked(&cli_argv(&["class", "show"], &[name])) {
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

    /// [`Self::read_op`] returning the raw `result` value instead of a deserialized
    /// struct — for the calls the lens re-renders into the CLI's text form, and for
    /// `types_declare --preview`, whose payload it reads field-by-field.
    ///
    /// Keeps `read_op`'s contract: `None` only for "this bridge does not register the
    /// op", never for a call that ran and failed.
    fn op_raw(
        &self,
        op: &str,
        params: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        match self.call_local(op, params)? {
            Err(error) if is_unknown_op(&error) => None,
            other => Some(other),
        }
    }

    /// [`Self::read_op`] for a **write**-locked mutation op: `None` means this bridge
    /// does not register the op (fall back to the CLI), `Some` means it ran.
    ///
    /// The unknown-op distinction matters more here than on a read. Falling back after
    /// a *genuine* failure would re-issue the write over the CLI, and a mutation that
    /// partially applied before failing would then be applied twice — so only the
    /// "this bridge never had the op" answer may retry.
    fn mutation_op(
        &self,
        op: &str,
        params: serde_json::Value,
    ) -> Option<Result<MutationOutcome, String>> {
        match self.call_local(op, params)? {
            Ok(value) => Some(Ok(mutation_outcome(&value))),
            Err(error) if is_unknown_op(&error) => None,
            Err(error) => Some(Err(error)),
        }
    }

    /// A write whose only caller-visible result is "did it commit". `None` when there
    /// is no socket path for it and the caller must spawn the CLI.
    fn wrote(&self, op: &str, params: serde_json::Value) -> Option<bool> {
        Some(match self.mutation_op(op, params)? {
            Ok(outcome) => outcome.success,
            Err(_) => false,
        })
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
            return syms.map(recovered_data_syms).unwrap_or_default();
        }
        self.py_json::<DataSymsJson>(DATA_SYMBOLS_PROGRAM)
            .map(recovered_data_syms)
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
        let out = self.run_out(&cli_argv(&["decompile"], &[name]));
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
        self.run_out(&cli_argv(&["decompile", "--addresses"], &[name]))
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
        let out = self.run_out(&cli_argv(
            &["decompile", "--addresses", "--format", "json"],
            &[id],
        ));
        let parsed: DecompiledFn = serde_json::from_str(&out).ok()?;
        if parsed.text.is_empty() {
            None
        } else {
            Some((parsed.function.name, parsed.function.address, parsed.text))
        }
    }

    /// `n` instructions in address-linear order starting *exactly* at `addr`
    /// (unlike `--count`, which slices from the containing function's start).
    /// Used to show the single instruction at an xref callsite.
    pub fn disasm_linear(&self, addr: &str, n: usize) -> String {
        if let Some(result) = self.op_raw(
            "disasm",
            serde_json::json!({"identifier": addr, "linear": n}),
        ) {
            return match result {
                Ok(value) => render_disasm_linear(&value),
                Err(error) => format!("✗ {error}"),
            };
        }
        let count = n.to_string();
        self.run(&cli_argv(&["disasm", "--linear", &count], &[addr]))
    }

    /// Xrefs (small; stdout is fine).
    pub fn xrefs(&self, name: &str) -> String {
        self.xrefs_checked(name)
            .unwrap_or_else(|error| format!("✗ {error}"))
    }

    pub fn xrefs_checked(&self, name: &str) -> Result<String, String> {
        // The hottest read in the lens: `usage::report` pairs one of these with up to
        // MAX_SITES `disasm_linear` calls, so on the CLI a single `p` peek paid over a
        // second in interpreter startup alone.
        let s = match self.op_raw("xrefs", serde_json::json!({"identifier": name})) {
            Some(result) => render_xrefs(&result?),
            None => self.run_out_checked(&cli_argv(&["xrefs"], &[name]))?,
        };
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
                .map(recovered_locals)
                .unwrap_or_default();
        }
        let out = self.run_out(&cli_argv(&["local", "list", "--format", "json"], &[func]));
        parse_local_list_json(&out)
    }

    /// Rename a local (`variable` = name or stable id). Returns true on a verified
    /// write. Mutates the live instance in-memory; persistence to the on-disk
    /// `.bndb` is a separate, deliberate `bn save` (not done per-rename).
    pub fn local_rename(&self, func: &str, old: &str, new: &str) -> bool {
        if let Some(committed) = self.wrote(
            "local_rename",
            serde_json::json!({"function": func, "variable": old, "new_name": new}),
        ) {
            return committed;
        }
        let out = self.run(&cli_argv(
            &["local", "rename", "--summary"],
            &[func, old, new],
        ));
        mutation_ok(&out)
    }

    /// Retype a local (`bn local retype`). Live in-memory; not persisted until a
    /// deliberate `bn save`. Returns whether it committed.
    pub fn local_retype(&self, func: &str, var: &str, new_type: &str) -> bool {
        if let Some(committed) = self.wrote(
            "local_retype",
            serde_json::json!({"function": func, "variable": var, "new_type": new_type}),
        ) {
            return committed;
        }
        let out = self.run(&cli_argv(
            &["local", "retype", "--summary"],
            &[func, var, new_type],
        ));
        mutation_ok(&out)
    }

    /// Validate a retype *without* committing (`--preview`: apply, diff, revert).
    /// `Ok(())` if the type parses and applies cleanly, else the parser error —
    /// so the composer can confirm a change before it touches the instance.
    pub fn local_retype_check(&self, func: &str, var: &str, new_type: &str) -> Result<(), String> {
        if let Some(result) = self.mutation_op(
            "local_retype",
            serde_json::json!({
                "function": func, "variable": var, "new_type": new_type, "preview": true
            }),
        ) {
            return match result {
                Ok(outcome) if outcome.success => Ok(()),
                Ok(outcome) => Err(clean_err(
                    &outcome.first_error.unwrap_or_else(|| "type rejected".into()),
                )),
                Err(error) => Err(clean_err(&error)),
            };
        }
        // stdout carries the `--summary` JSON (with `first_error`) even when a
        // bad type makes bn exit non-zero, so read it regardless of exit status.
        let out = self.run_stdout(&cli_argv(
            &["local", "retype", "--preview", "--summary"],
            &[func, var, new_type],
        ));
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
        if let Some(committed) = self.wrote(
            "rename_symbol",
            serde_json::json!({"identifier": ident, "new_name": new, "kind": "function"}),
        ) {
            return committed;
        }
        let out = self.run(&cli_argv(
            &["rename", "--kind", "function", "--summary"],
            &[ident, new],
        ));
        mutation_ok(&out)
    }

    /// Set an address comment (the note shown on that line).
    pub fn comment_set_addr(&self, addr: &str, text: &str) -> bool {
        if let Some(committed) = self.wrote(
            "set_comment",
            serde_json::json!({"address": addr, "comment": text}),
        ) {
            return committed;
        }
        let out = self.run(&cli_argv(&["comment", "set", "--summary"], &[addr, text]));
        mutation_ok(&out)
    }

    /// The comment currently at `addr` (`comment get`), for edit-in-place.
    /// `None` when there is none (or on failure).
    pub fn comment_get_addr(&self, addr: &str) -> Option<String> {
        let value = match self.op_raw("get_comment", serde_json::json!({"address": addr})) {
            // A read that genuinely failed (an unmapped address) has no comment to
            // report — same answer the CLI path gives by failing to parse.
            Some(result) => result.ok()?,
            None => {
                let out = self.run_stdout(&cli_argv(
                    &["comment", "get", "--format", "json"],
                    &[addr],
                ));
                serde_json::from_str(out.trim()).ok()?
            }
        };
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
        let value = match self.op_raw("get_comment", serde_json::json!({"function": func})) {
            Some(Ok(value)) => value,
            Some(Err(_)) => return FuncComment::default(),
            None => {
                // `--function` takes a value, so a dash-leading function name needs
                // the `=` form rather than the `--` separator.
                let function = flag_eq("--function", func);
                let out = self.run_stdout(&["comment", "get", &function, "--format", "json"]);
                match serde_json::from_str::<serde_json::Value>(out.trim()) {
                    Ok(value) => value,
                    Err(_) => return FuncComment::default(),
                }
            }
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
        if let Some(committed) = self.wrote(
            "set_comment",
            serde_json::json!({"function": func, "comment": text}),
        ) {
            return committed;
        }
        let function = flag_eq("--function", func);
        let out = self.run(&cli_argv(
            &["comment", "set", &function, "--summary"],
            &[text],
        ));
        mutation_ok(&out)
    }

    /// Delete the comment at `addr` (clearing a comment edited down to empty).
    pub fn comment_delete_addr(&self, addr: &str) -> bool {
        if let Some(committed) =
            self.wrote("delete_comment", serde_json::json!({"address": addr}))
        {
            return committed;
        }
        let out = self.run(&cli_argv(&["comment", "delete", "--summary"], &[addr]));
        mutation_ok(&out)
    }

    /// Delete a function's documentation comment.
    pub fn comment_delete_func(&self, func: &str) -> bool {
        if let Some(committed) =
            self.wrote("delete_comment", serde_json::json!({"function": func}))
        {
            return committed;
        }
        let function = flag_eq("--function", func);
        let out = self.run(&["comment", "delete", &function, "--summary"]);
        mutation_ok(&out)
    }

    /// Add a tag of `ty` (e.g. `Bookmarks`) at an address, with optional note.
    pub fn tag_add_addr(&self, addr: &str, ty: &str, data: &str) -> bool {
        if let Some(committed) = self.wrote(
            "tag_add",
            serde_json::json!({"address": addr, "type": ty, "data": data}),
        ) {
            return committed;
        }
        // The note and the tag type are flag VALUES: a user note like `-> see
        // parse_hdr` is rejected outright as `--data <note>`, and `--` cannot fix a
        // flag value.
        let (ty, data) = (flag_eq("--type", ty), flag_eq("--data", data));
        let out = self.run(&cli_argv(&["tag", "add", &ty, &data, "--summary"], &[addr]));
        mutation_ok(&out)
    }

    /// Add a tag of `ty` on a whole function, with optional note.
    pub fn tag_add_func(&self, func: &str, ty: &str, data: &str) -> bool {
        if let Some(committed) = self.wrote(
            "tag_add",
            serde_json::json!({"function": func, "type": ty, "data": data}),
        ) {
            return committed;
        }
        let (function, ty, data) = (
            flag_eq("--function", func),
            flag_eq("--type", ty),
            flag_eq("--data", data),
        );
        let out = self.run(&["tag", "add", &function, &ty, &data, "--summary"]);
        mutation_ok(&out)
    }

    /// Hex+ASCII dump of `length` bytes at `addr`.
    pub fn read(&self, addr: &str, length: usize) -> String {
        let s = match self.read_bytes(addr, length) {
            Some(result) => result.unwrap_or_else(|error| format!("✗ {error}")),
            None => {
                let len = length.to_string();
                self.run(&cli_argv(&["read", "--length", &len], &[addr]))
            }
        };
        if s.trim().is_empty() {
            "(no data)".into()
        } else {
            s
        }
    }

    /// The `read` op rendered as the CLI's hexdump, or `None` when there is no socket
    /// path for it. Shared by [`Self::read`] and [`Self::read_dump`], which differ only
    /// in how they report a failure.
    fn read_bytes(&self, addr: &str, length: usize) -> Option<Result<String, String>> {
        Some(
            match self.op_raw(
                "read",
                serde_json::json!({"address": addr, "length": length}),
            )? {
                Ok(value) => Ok(render_read(&value).unwrap_or_default()),
                Err(error) => Err(error),
            },
        )
    }

    /// Like [`Self::read`] but via `--out`, so a large window (the linear data
    /// view can request several KB) doesn't trip bn's stdout spill envelope.
    /// Empty on failure.
    pub fn read_dump(&self, addr: &str, length: usize) -> String {
        if let Some(result) = self.read_bytes(addr, length) {
            return result.unwrap_or_default();
        }
        let len = length.to_string();
        let out = self.run_out(&cli_argv(&["read", "--length", &len], &[addr]));
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
    let warnings: Vec<String> = value
        .get("warnings")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|warning| format!("warning: {warning}"))
                .collect()
        })
        .unwrap_or_default();
    if !warnings.is_empty() {
        body = format!("{body}\n\n{}", warnings.join("\n"));
    }
    format!("{}{body}", resolution_note(value))
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

/// Parse a bn-rendered address (`0x…`, else decimal), the way the CLI's own
/// renderers do (`int(s, 16) if s.startswith("0x") else int(s)`).
fn parse_render_addr(text: &str) -> Option<u64> {
    let text = text.trim();
    match text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    // Byte-indexed slicing below would panic mid-codepoint on non-ASCII input.
    if !hex.is_ascii() || !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}

/// Render a socket `read` result as the hexdump the CLI printed
/// (`formatters.py::_render_read_text`): `<addr>: <16 hex bytes>  <ascii>`, 16 bytes
/// per row, then any short-read note.
///
/// The column layout is a contract, not decoration — the linear data view scrapes
/// these rows — so the hex field keeps the CLI's `width * 3 - 1` padding. `None` when
/// the payload carries no decodable `hex`, so the caller can fall back.
fn render_read(value: &serde_json::Value) -> Option<String> {
    const WIDTH: usize = 16;
    let data = decode_hex(value.get("hex").and_then(serde_json::Value::as_str)?)?;
    let base = parse_render_addr(
        value
            .get("address")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("0x0"),
    )
    .unwrap_or(0);
    let mut lines: Vec<String> = data
        .chunks(WIDTH)
        .enumerate()
        .map(|(row, chunk)| {
            let hex_bytes = chunk
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            let ascii: String = chunk
                .iter()
                .map(|&byte| {
                    if (0x20..0x7f).contains(&byte) {
                        byte as char
                    } else {
                        '.'
                    }
                })
                .collect();
            // saturating: a window at the very top of the address space must clamp,
            // not wrap in release and panic in a debug test.
            let at = base.saturating_add((row * WIDTH) as u64);
            format!("{at:08x}: {hex_bytes:<width$}  {ascii}", width = WIDTH * 3 - 1)
        })
        .collect();
    if lines.is_empty() {
        lines.push(format!("{base:08x}: (no bytes)"));
    }
    if let Some(note) = value
        .get("note")
        .and_then(serde_json::Value::as_str)
        .filter(|note| !note.is_empty())
    {
        lines.push(String::new());
        lines.push(format!("note: {note}"));
    }
    Some(terminated(lines.join("\n")))
}

/// Give a socket render the trailing newline the CLI path carried.
///
/// This is not cosmetic. What the CLI handed the lens was a *printed* body, so it
/// always ended in `\n`, and `syntax::tokenize_plain` splits on `'\n'` — meaning that
/// newline contributes a final empty line. A render without it is one line shorter
/// than the same read through the CLI, which showed up in dogfooding as a `1/7`
/// footer where the CLI path said `1/8`. Reproduce the byte the consumer saw.
fn terminated(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Render a socket `disasm --linear` result the way the CLI did
/// (`formatters.py::_render_disasm_linear_text`): the `// bn:` note, then the
/// address/bytes/mnemonic lines. The leading `//` is load-bearing — `usage::disasm_line`
/// skips comment lines to find the instruction.
fn render_disasm_linear(value: &serde_json::Value) -> String {
    let body = text_field(value);
    terminated(
        match value
            .get("note")
            .and_then(serde_json::Value::as_str)
            .filter(|note| !note.is_empty())
        {
            Some(note) if body.is_empty() => format!("// bn: {note}"),
            Some(note) => format!("// bn: {note}\n{body}"),
            None => body,
        },
    )
}

/// Render a socket `type_info` result as `types show` printed it
/// (`formatters.py::_render_type_info_text`): the rendered layout, else the bare
/// declaration. `None` when neither is present, so the caller can fall back.
///
/// Deliberately NOT run through [`terminated`]: `type_show` splits with `.lines()`,
/// which discards a trailing newline, and an empty render must stay empty so the
/// `(no layout for …)` placeholder still fires.
fn render_type_info(value: &serde_json::Value) -> Option<String> {
    ["layout", "decl"].into_iter().find_map(|key| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

/// One caller group in an xrefs listing: the referencing function (or, for a ref with
/// no containing function, its resolved symbol/section label) and every site under it.
struct XrefGroup {
    /// `None` for a ref with no containing function — which is what tells the header
    /// to say "locations" rather than miscounting them as functions.
    caller_address: Option<String>,
    caller_name: Option<String>,
    context: Option<serde_json::Value>,
    sites: Vec<String>,
}

/// A concise fallback label (symbol name, else section name) for a ref with no
/// containing function, so it doesn't render as a bare `<unknown>`
/// (`formatters.py::_unknown_ref_label`).
fn unknown_ref_label(context: Option<&serde_json::Value>) -> Option<String> {
    let context = context?;
    if let Some(name) = context
        .get("symbol")
        .and_then(|symbol| symbol.get("name"))
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
    {
        return Some(name.to_string());
    }
    context
        .get("sections")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .find_map(|section| {
            section
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
}

/// Group refs by referencing caller, preserving first-seen order
/// (`formatters.py::_group_refs_by_caller`).
///
/// Refs with no containing function key on their own resolved label so refs under
/// *different* symbols/sections don't collapse into one group stamped with the first
/// ref's label; a label-less ref keys on its own address, so each renders as its own
/// `<unknown>` line rather than being coalesced.
fn group_refs_by_caller(refs: &[serde_json::Value]) -> Vec<XrefGroup> {
    let mut groups: Vec<XrefGroup> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let field = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    for reference in refs {
        if !reference.is_object() {
            continue;
        }
        let context = reference.get("context").cloned().filter(|c| !c.is_null());
        let site = field(reference, "address").unwrap_or_else(|| "<unknown>".into());
        let caller = reference
            .get("caller_function")
            .filter(|caller| caller.is_object());
        let (key, caller_address, caller_name) = match caller {
            Some(caller) => {
                let address = field(caller, "address");
                let name = field(caller, "name");
                (
                    format!(
                        "fn\u{1}{}\u{1}{}",
                        address.as_deref().unwrap_or(""),
                        name.as_deref().unwrap_or("")
                    ),
                    address,
                    name,
                )
            }
            None => {
                let label =
                    unknown_ref_label(context.as_ref()).or_else(|| field(reference, "function"));
                let key = match &label {
                    Some(label) => format!("label\u{1}{label}"),
                    None => format!("site\u{1}{site}"),
                };
                (key, None, label)
            }
        };
        match index.get(&key) {
            Some(&at) => groups[at].sites.push(site),
            None => {
                index.insert(key, groups.len());
                groups.push(XrefGroup {
                    caller_address,
                    caller_name,
                    context,
                    sites: vec![site],
                });
            }
        }
    }
    groups
}

/// Render a socket `xrefs` result as the text the CLI printed
/// (`formatters.py::_render_xrefs_text`).
///
/// `usage::parse_xrefs` scrapes this — the `code refs`/`data refs` section headers,
/// the `- none` sentinel and the `<caller addr>  <name>  (N sites: …)` row shape are
/// all contract. No display cap is applied because the CLI path this replaces passes
/// `--out`, and `--out` means an uncapped body (`cli.py::_effective_limit`).
fn render_xrefs(value: &serde_json::Value) -> String {
    let no_refs = Vec::new();
    let items = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .unwrap_or(&no_refs);
    let bucket = |kind: &str| -> Vec<serde_json::Value> {
        // The op ships one `items` list tagged by `kind`; the deprecated dual-array
        // shape is still emitted elsewhere, so honour it when present.
        value
            .get(if kind == "code" { "code_refs" } else { "data_refs" })
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_else(|| {
                items
                    .iter()
                    .filter(|item| {
                        item.get("kind").and_then(serde_json::Value::as_str) == Some(kind)
                    })
                    .cloned()
                    .collect()
            })
    };
    let code_refs = bucket("code");
    let data_refs = bucket("data");
    // Totals come from the full-set summary counts when present, so the header stays
    // honest even though a page may carry fewer refs than it counts.
    let total = |key: &str, refs: &[serde_json::Value]| -> u64 {
        value
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(refs.len() as u64)
    };
    let total_code = total("code_ref_count", &code_refs);
    let total_data = total("data_ref_count", &data_refs);

    let render_group = |refs: &[serde_json::Value], total: u64, label: &str| -> Vec<String> {
        let groups = group_refs_by_caller(refs);
        if groups.is_empty() {
            return vec![format!("{label}:"), "- none".into()];
        }
        let site_word = if total == 1 { "site" } else { "sites" };
        // Only call the groups "functions" when every one really is one; a
        // function-less bucket must not be miscounted as a function.
        let all_functions = groups
            .iter()
            .all(|group| group.caller_address.is_some());
        let group_word = match (all_functions, groups.len() == 1) {
            (true, true) => "function",
            (true, false) => "functions",
            (false, true) => "location",
            (false, false) => "locations",
        };
        let mut lines = vec![format!(
            "{label}: {total} {site_word} across {} {group_word}",
            groups.len()
        )];
        for group in &groups {
            let caller_address = group.caller_address.as_deref().unwrap_or("<unknown>");
            let caller_name = group
                .caller_name
                .clone()
                .or_else(|| unknown_ref_label(group.context.as_ref()))
                .unwrap_or_else(|| "<unknown>".into());
            let suffix = if group.sites.len() == 1 {
                format!("(1 site: {})", group.sites[0])
            } else {
                format!("({} sites: {})", group.sites.len(), group.sites.join(", "))
            };
            lines.push(format!("  {caller_address}  {caller_name}  {suffix}"));
        }
        lines
    };

    let address = value
        .get("address")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>");
    let mut lines = vec![
        format!("xrefs to {address} ({total_code} code, {total_data} data)"),
        String::new(),
    ];
    if value
        .get("import_resolved")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        let scanned = if value
            .get("code_refs_scanned")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            " (scanned)"
        } else {
            ""
        };
        let name = value
            .get("import_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<unknown>");
        lines.insert(0, format!("import: {name}{scanned}"));
        lines.insert(1, String::new());
    }
    // An ambiguous same-name collision (thunk vs real): surface the note so a
    // zero-caller member is never mistaken for dead code.
    if let Some(note) = value
        .get("ambiguous_symbol")
        .filter(|amb| amb.is_object())
        .and_then(|amb| amb.get("note"))
        .and_then(serde_json::Value::as_str)
    {
        lines.insert(0, format!("note: {note}"));
        lines.insert(1, String::new());
    }
    if let Some(resolved) = value
        .get("resolved_symbol")
        .filter(|sym| sym.get("kind").and_then(serde_json::Value::as_str) == Some("data"))
    {
        let name = resolved
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let at = resolved
            .get("address")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        lines.insert(0, format!("note: resolved '{name}' as a data symbol @ {at}"));
        lines.insert(1, String::new());
    }
    lines.extend(render_group(&code_refs, total_code, "code refs"));
    lines.push(String::new());
    lines.extend(render_group(&data_refs, total_data, "data refs"));
    terminated(lines.join("\n"))
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
        capture_dir, cli_argv, decompile_render, flag_eq, is_unknown_op, json_escape_ascii,
        mutation_outcome, parse_local_list_json, parse_types_page, push_mark, recovered_class,
        recovered_data_syms, render_disasm_linear, render_read, render_sections, render_type_info,
        render_xrefs, text_field, with_capture, AnalysisState, Bn, ClassItemJson, CommentListJson,
        DataSymsJson, TagListJson,
    };
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    // ---- #26: one malformed element must not drop the whole list ----

    #[test]
    fn issue26_locals_survive_one_element_missing_a_name() {
        // Before `LocalVariable.name` carried `#[serde(default)]`, serde aborted the
        // entire `Vec` here and `local_list` flattened the error to an empty list —
        // no Local hotspots, and `n` on a variable silently renaming the FUNCTION.
        let json = r#"{
            "function": {"address": "0x401000", "name": "parse_header"},
            "locals": [
                {"local_id": "0x401000:local:stack:-32:0:1", "name": "header_len",
                 "type": "uint32_t", "source_type": "StackVariableSourceType",
                 "storage": -32, "is_parameter": false},
                {"local_id": "0x401000:local:stack:-24:0:2",
                 "type": "void*", "source_type": "StackVariableSourceType",
                 "storage": -24, "is_parameter": false},
                {"local_id": "0x401000:param:reg:0:0:3", "name": "buf",
                 "type": "uint8_t*", "source_type": "RegisterVariableSourceType",
                 "storage": 0, "is_parameter": true}
            ]
        }"#;
        let locals = parse_local_list_json(json);
        // The nameless element costs only itself.
        let names: Vec<&str> = locals.iter().map(|local| local.name.as_str()).collect();
        assert_eq!(names, ["header_len", "buf"]);
        assert_eq!(locals[0].storage, -32);
        assert!(locals[1].is_parameter);
    }

    #[test]
    fn issue26_data_symbols_survive_one_element_missing_a_field() {
        // An empty data-symbol map is the regression this read exists to prevent:
        // renamed globals stop being hotspots, so `w`/`b` skip them and `p`/`x`
        // cannot target them.
        let payload = serde_json::from_str::<DataSymsJson>(
            r#"{"syms":[
                {"a":"0x415200","n":"g_config"},
                {"n":"g_missing_address"},
                {"a":"0x415280"},
                {"a":"0x4152c0","n":"g_session_table"}
            ]}"#,
        )
        .expect("a short element must not abort the list");
        assert_eq!(
            recovered_data_syms(payload),
            [
                ("0x415200".to_string(), "g_config".to_string()),
                ("0x4152c0".to_string(), "g_session_table".to_string()),
            ],
            "elements missing either half are dropped; the rest survive"
        );
    }

    // ---- #13: `--` before the first untrusted positional ----

    /// Assert an argv is option-injection safe: every flag precedes `--`, nothing
    /// after `--` is parsed as one, and the untrusted operands are exactly `expected`.
    fn assert_guarded(argv: &[impl AsRef<str> + std::fmt::Debug], expected: &[&str]) {
        let at = argv
            .iter()
            .position(|arg| arg.as_ref() == "--")
            .unwrap_or_else(|| panic!("no `--` separator in {argv:?}"));
        let operands: Vec<&str> = argv[at + 1..].iter().map(AsRef::as_ref).collect();
        assert_eq!(
            operands, expected,
            "everything after `--` must be exactly the untrusted operands"
        );
        // A `--` placed before the flags would make argparse read `--summary` as a
        // positional, so the separator has to come LAST among the trusted tokens.
        assert!(
            !argv[at + 1..].is_empty(),
            "a bare trailing `--` is pointless"
        );
    }

    #[test]
    fn issue13_cli_argv_puts_the_separator_before_untrusted_positionals() {
        // A comment body opening with a dash is ordinary prose. Without `--`,
        // argparse takes it as a flag and the write fails with "the following
        // arguments are required: text" — reported as a bare mutation failure.
        let argv = cli_argv(&["comment", "set", "--summary"], &["0x401000", "-wip"]);
        assert_eq!(argv, ["comment", "set", "--summary", "--", "0x401000", "-wip"]);
        assert_guarded(&argv, &["0x401000", "-wip"]);

        // A function named `--out` must not be able to steer bn's own `--out`.
        assert_eq!(
            cli_argv(&["decompile", "--addresses"], &["--out"]),
            ["decompile", "--addresses", "--", "--out"]
        );

        // No operands, no separator: a bare `--` would be noise.
        assert_eq!(cli_argv(&["xrefs"], &[]), ["xrefs"]);

        // The health key still names the command, not the separator.
        assert_eq!(Bn::command_key(&cli_argv(&["xrefs"], &["parse"])), "xrefs");
        assert_eq!(
            Bn::command_key(&cli_argv(&["comment", "set", "--summary"], &["0x1000", "x"])),
            "comment set"
        );
    }

    #[test]
    fn issue13_the_capture_flag_stays_on_the_option_side_of_the_separator() {
        // Caught by `issue13_real_read_paths_emit_a_guarded_argv`: `run_out` used to
        // APPEND `--out <path>`, which after a `--` separator arrives as two stray
        // positionals instead of bn's own flag — breaking every large read.
        let guarded = cli_argv(&["decompile", "--addresses"], &["parse_header"]);
        assert_eq!(
            with_capture(&guarded, "/tmp/cap/0.out"),
            [
                "decompile",
                "--addresses",
                "--out",
                "/tmp/cap/0.out",
                "--",
                "parse_header"
            ]
        );
        // Nothing to splice past when there is no separator.
        assert_eq!(
            with_capture(&["py", "exec", "--format", "json"], "/tmp/cap/1.out"),
            ["py", "exec", "--format", "json", "--out", "/tmp/cap/1.out"]
        );
    }

    #[test]
    fn issue13_dash_leading_flag_values_use_the_equals_form() {
        // `--` cannot rescue a flag VALUE: argparse fails with "expected one
        // argument" before it ever reaches the separator. `--flag=value` is the only
        // form that accepts a dash-leading value.
        assert_eq!(flag_eq("--data", "-> see parse_hdr"), "--data=-> see parse_hdr");
        assert_eq!(flag_eq("--function", "--weird"), "--function=--weird");
        assert!(!flag_eq("--type", "Bookmarks").contains(' '));
    }

    /// A `Bn` pinned to the CLI path — `client: None`, so the socket branch is skipped
    /// regardless of how many bridges happen to be live on this host — whose binary is
    /// a script that records the argv it was handed.
    fn argv_recorder(dir: &Path) -> (Bn, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let recorded = dir.join("argv");
        let script = dir.join("fake-bn");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexit 1\n",
                recorded.display()
            ),
        )
        .expect("write recorder script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("chmod recorder script");
        let bn = Bn {
            bin: script.to_string_lossy().into_owned(),
            instance: None,
            target: None,
            client: None,
            health: Arc::new(Mutex::new(None)),
        };
        (bn, recorded)
    }

    fn recorded_argv(at: &Path) -> Vec<String> {
        std::fs::read_to_string(at)
            .expect("the recorder script ran")
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn issue13_real_write_paths_emit_a_guarded_argv() {
        // The pure builder is only half the fix — this pins the actual call sites, so
        // a future edit that hand-rolls an argv again fails here.
        let dir = scratch_dir("argv");
        let (bn, recorded) = argv_recorder(&dir);

        bn.comment_set_addr("0x401000", "-- checked, bounds fine");
        assert_guarded(
            &recorded_argv(&recorded),
            &["0x401000", "-- checked, bounds fine"],
        );

        bn.symbol_rename("-t", "parse_header");
        assert_guarded(&recorded_argv(&recorded), &["-t", "parse_header"]);

        // A dash-leading tag note rides as `--data=<note>`, never as a bare value.
        bn.tag_add_addr("0x401000", "Bookmarks", "-> see parse_hdr");
        let argv = recorded_argv(&recorded);
        assert!(
            argv.contains(&"--data=-> see parse_hdr".to_string()),
            "tag note must use the `=` form: {argv:?}"
        );
        assert_guarded(&argv, &["0x401000"]);

        // A function-scoped comment has NO untrusted positional but two untrusted
        // values, so it must carry both in `=` form.
        bn.comment_set_func("--weird", "-wip");
        let argv = recorded_argv(&recorded);
        assert!(argv.contains(&"--function=--weird".to_string()), "{argv:?}");
        assert_guarded(&argv, &["-wip"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn issue13_real_read_paths_emit_a_guarded_argv() {
        let dir = scratch_dir("argv-read");
        let (bn, recorded) = argv_recorder(&dir);

        bn.decompile("--out");
        let argv = recorded_argv(&recorded);
        // bn's own `--out` capture flag is appended by `run_out` AFTER the operands,
        // so the guard must not swallow it into the operand list.
        assert!(argv.contains(&"--out".to_string()));
        let at = argv.iter().position(|arg| arg == "--").expect("separator");
        assert_eq!(argv[at + 1], "--out", "the function name is an operand");

        bn.type_show("-2");
        assert_guarded(&recorded_argv(&recorded), &["-2"]);

        bn.read("-1", 16);
        assert_guarded(&recorded_argv(&recorded), &["-1"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- #27: captures live in a private per-process directory ----

    /// A fresh scratch directory for a test, alongside the capture dir so it inherits
    /// the same `TMPDIR` honouring.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bn-lens-test-{}-{tag}-{:016x}",
            std::process::id(),
            super::capture_nonce()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn issue27_capture_paths_live_in_a_private_per_process_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = capture_dir().expect("a private capture directory must be creatable");

        // 0700: a capture holds decompiled output of the target under audit, so no
        // other local user may open it whatever its name.
        let mode = std::fs::metadata(dir)
            .expect("capture dir exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "capture dir must be 0700, got {mode:o}");

        // Per process, and under the honoured temp root.
        assert!(dir.starts_with(std::env::temp_dir()));
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("dir name");
        let prefix = format!("bn-lens-{}-", std::process::id());
        assert!(name.starts_with(&prefix), "{name} lacks the pid prefix");
        // The nonce is what defeats pre-creation: the old scheme's name was fully
        // derivable from (pid, call ordinal).
        assert_eq!(
            name.len(),
            prefix.len() + 16,
            "{name} must carry a 64-bit nonce"
        );

        // Every capture path is inside it, and no two collide.
        let first = super::unique_tmp();
        let second = super::unique_tmp();
        assert_ne!(first, second);
        for path in [&first, &second] {
            assert_eq!(
                Path::new(path).parent(),
                Some(dir.as_path()),
                "{path} escaped the private directory"
            );
        }
    }

    // ---- #21: socket writes and hot reads must read the same as the CLI's ----

    #[test]
    fn issue21_mutation_outcome_matches_the_clis_summary_collapse() {
        // `--summary` is a CLI-side transform, so a socket write has to derive the
        // same verdict from the raw result or it would disagree with the CLI.
        let committed = serde_json::json!({
            "preview": false, "success": true, "committed": true,
            "results": [{"op": "set_comment", "status": "verified"}],
        });
        let outcome = mutation_outcome(&committed);
        assert!(outcome.success);
        assert_eq!(outcome.changed_count, 1);
        assert_eq!(outcome.first_error, None);

        // A no-op verifies nothing but is still a success (nothing to change).
        let noop = serde_json::json!({
            "success": true, "committed": true,
            "results": [{"op": "rename_symbol", "status": "noop"}],
        });
        assert!(mutation_outcome(&noop).success);
        assert_eq!(mutation_outcome(&noop).changed_count, 0);

        // The load-bearing case: `success` alone reads TRUE here, but a
        // verification_failed row means the write did not land. Trusting `success`
        // would make the lens retext a rename that never happened.
        let unverified = serde_json::json!({
            "success": true, "committed": false, "rolled_back": true,
            "results": [{
                "op": "local_rename", "status": "verification_failed",
                "message": "variable still named var_18 after rename",
            }],
        });
        let outcome = mutation_outcome(&unverified);
        assert!(!outcome.success);
        assert_eq!(
            outcome.first_error.as_deref(),
            Some("variable still named var_18 after rename")
        );

        // A failure whose only explanation is top-level (a revert that failed after
        // every op verified, so no row is in the failed set).
        let top_level = serde_json::json!({
            "success": false, "committed": false,
            "message": "rollback failed: the view may be left modified",
            "results": [{"op": "types_declare", "status": "verified"}],
        });
        assert_eq!(
            mutation_outcome(&top_level).first_error.as_deref(),
            Some("rollback failed: the view may be left modified")
        );

        // An absent `success` defaults to true, as the CLI does — a shape change
        // must not read as a failed write.
        assert!(mutation_outcome(&serde_json::json!({"results": []})).success);
        // …and a failure with nothing to quote still names itself.
        assert_eq!(
            mutation_outcome(&serde_json::json!({"success": false}))
                .first_error
                .as_deref(),
            Some("mutation failed")
        );
    }

    #[test]
    fn issue21_render_read_reproduces_the_clis_hexdump_columns() {
        // The linear data view scrapes these rows, so the column layout is a
        // contract: `<addr>: <16 hex bytes padded to 47>  <ascii>`.
        let value = serde_json::json!({
            "address": "0x415200",
            "length": 20,
            "hex": "89504e470d0a1a0a0000000d49484452007f2041",
        });
        let rendered = render_read(&value).expect("decodable hex renders");
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0],
            "00415200: 89 50 4e 47 0d 0a 1a 0a 00 00 00 0d 49 48 44 52  .PNG........IHDR"
        );
        // The second row is short; the hex field still pads to full width so the
        // ascii column stays aligned.
        assert_eq!(
            lines[1],
            "00415210: 00 7f 20 41                                      .. A"
        );
        // Non-printables and DEL render as `.`; 0x20 and 0x7e are the inclusive
        // bounds of the printable range.
        let bounds = serde_json::json!({"address": "0x1000", "hex": "1f207e7f"});
        assert!(render_read(&bounds).unwrap().ends_with(". ~.\n"));

        // A short read appends the note the CLI printed.
        let short = serde_json::json!({
            "address": "0x415200", "hex": "0011", "short_read": true,
            "note": "short read: requested 8 bytes, only 2 mapped from 0x415200",
        });
        let rendered = render_read(&short).unwrap();
        assert!(rendered
            .ends_with("\n\nnote: short read: requested 8 bytes, only 2 mapped from 0x415200\n"));

        // Zero bytes is a rendered row, not an empty string.
        assert_eq!(
            render_read(&serde_json::json!({"address": "0x1000", "hex": ""})).unwrap(),
            "00001000: (no bytes)\n"
        );
        // Undecodable payloads fall back rather than panicking on a byte slice.
        assert!(render_read(&serde_json::json!({"address": "0x1000", "hex": "abc"})).is_none());
        assert!(render_read(&serde_json::json!({"address": "0x1000", "hex": "zz"})).is_none());
        assert!(render_read(&serde_json::json!({"address": "0x1000", "hex": "é9"})).is_none());
        assert!(render_read(&serde_json::json!({})).is_none());
        // An address at the very top must clamp, not wrap (release has no
        // overflow-checks, so a debug test is the only guard).
        let top = serde_json::json!({
            "address": "0xffffffffffffffff",
            "hex": "00112233445566778899aabbccddeeff00",
        });
        assert_eq!(render_read(&top).unwrap().lines().count(), 2);
    }

    #[test]
    fn issue21_render_xrefs_keeps_the_shape_usage_parses() {
        // `usage::parse_xrefs` keys on the section headers and the
        // `<caller addr>  <name>  (N sites: …)` row shape.
        let value = serde_json::json!({
            "address": "0x4016d0",
            "code_ref_count": 5,
            "data_ref_count": 1,
            "items": [
                {"kind": "code", "address": "0x40d214",
                 "caller_function": {"address": "0x40d040", "name": "add_resource_record"}},
                {"kind": "code", "address": "0x40d620",
                 "caller_function": {"address": "0x40d040", "name": "add_resource_record"}},
                {"kind": "code", "address": "0x41028c",
                 "caller_function": {"address": "0x410240", "name": "expand_buf"}},
                {"kind": "code", "address": "0x4102a0",
                 "caller_function": {"address": "0x410240", "name": "expand_buf"}},
                {"kind": "code", "address": "0x4102b4",
                 "caller_function": {"address": "0x410240", "name": "expand_buf"}},
                {"kind": "data", "address": "0x4152a0",
                 "context": {"symbol": {"name": "ptr_table"}}}
            ],
        });
        let rendered = render_xrefs(&value);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "xrefs to 0x4016d0 (5 code, 1 data)");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "code refs: 5 sites across 2 functions");
        assert_eq!(
            lines[3],
            "  0x40d040  add_resource_record  (2 sites: 0x40d214, 0x40d620)"
        );
        assert_eq!(
            lines[4],
            "  0x410240  expand_buf  (3 sites: 0x41028c, 0x4102a0, 0x4102b4)"
        );
        assert_eq!(lines[5], "");
        // A function-less data ref is a "location", never miscounted as a function.
        assert_eq!(lines[6], "data refs: 1 site across 1 location");
        assert_eq!(lines[7], "  <unknown>  ptr_table  (1 site: 0x4152a0)");
    }

    #[test]
    fn issue21_render_xrefs_reports_absence_the_way_the_cli_did() {
        let empty = serde_json::json!({"address": "0x400238", "items": []});
        assert_eq!(
            render_xrefs(&empty),
            "xrefs to 0x400238 (0 code, 0 data)\n\ncode refs:\n- none\n\ndata refs:\n- none\n"
        );

        // A same-name thunk/real collision: the note is what stops a zero-caller
        // member reading as dead code.
        let ambiguous = serde_json::json!({
            "address": "0x401000",
            "ambiguous_symbol": {"note": "2 symbols named parse_header; showing the one at 0x401000"},
            "items": [],
        });
        assert_eq!(
            render_xrefs(&ambiguous).lines().next(),
            Some("note: 2 symbols named parse_header; showing the one at 0x401000")
        );

        // Refs under DIFFERENT sections must not collapse into one group stamped
        // with the first ref's label.
        let sections = serde_json::json!({
            "address": "0x415200",
            "items": [
                {"kind": "data", "address": "0x420000",
                 "context": {"sections": [{"name": ".data.rel.ro"}]}},
                {"kind": "data", "address": "0x430000",
                 "context": {"sections": [{"name": ".init_array"}]}}
            ],
        });
        let rendered = render_xrefs(&sections);
        assert!(rendered.contains("data refs: 2 sites across 2 locations"));
        assert!(rendered.contains("  <unknown>  .data.rel.ro  (1 site: 0x420000)"));
        assert!(rendered.contains("  <unknown>  .init_array  (1 site: 0x430000)"));

        // A label-less ref keys on its own address, so two of them stay distinct
        // rather than coalescing into one `<unknown>` row.
        let bare = serde_json::json!({
            "address": "0x415200",
            "items": [
                {"kind": "data", "address": "0x420000"},
                {"kind": "data", "address": "0x430000"}
            ],
        });
        assert!(render_xrefs(&bare).contains("data refs: 2 sites across 2 locations"));

        // The deprecated dual-array shape still renders (function info embeds it).
        let dual = serde_json::json!({
            "address": "0x401000",
            "code_refs": [{"address": "0x402000",
                           "caller_function": {"address": "0x401f00", "name": "main"}}],
            "data_refs": [],
        });
        assert!(render_xrefs(&dual).contains("code refs: 1 site across 1 function"));
    }

    #[test]
    fn issue21_socket_renders_carry_the_clis_trailing_newline() {
        // Found by dogfooding, not by reading: `syntax::tokenize_plain` splits on
        // '\n', so the newline bn's CLI printed contributed a final empty line. A
        // render without it made the same xrefs read one line shorter over the
        // socket than over the CLI — the viewer footer said `1/7` where the CLI path
        // said `1/8`.
        let xrefs = render_xrefs(&serde_json::json!({"address": "0x401160", "items": []}));
        assert!(xrefs.ends_with('\n'), "xrefs render: {xrefs:?}");
        assert_eq!(
            xrefs.split('\n').count(),
            xrefs.lines().count() + 1,
            "the trailing newline must yield the extra split element the CLI path had"
        );

        let read = render_read(&serde_json::json!({"address": "0x1000", "hex": "00"})).unwrap();
        assert!(read.ends_with('\n'), "read render: {read:?}");

        let disasm = render_disasm_linear(&serde_json::json!({"text": "0040d214  nop"}));
        assert!(disasm.ends_with('\n'), "disasm render: {disasm:?}");

        // `type_info` is the deliberate exception: `type_show` splits with `.lines()`
        // and needs an empty render to stay empty so its placeholder still fires.
        assert!(render_type_info(&serde_json::json!({"layout": ""})).is_none());
    }

    #[test]
    fn issue21_render_type_info_and_disasm_match_their_cli_text() {
        // `types show` prints the rendered layout, falling back to the declaration.
        let layout = serde_json::json!({
            "layout": "struct packet_hdr // size=0x10\n    0x0  uint32_t magic;",
            "decl": "struct packet_hdr",
        });
        assert!(render_type_info(&layout).unwrap().starts_with("struct packet_hdr //"));
        assert_eq!(
            render_type_info(&serde_json::json!({"decl": "typedef uint32_t handle_t"})).as_deref(),
            Some("typedef uint32_t handle_t")
        );
        assert!(render_type_info(&serde_json::json!({"layout": ""})).is_none());
        assert!(render_type_info(&serde_json::json!({})).is_none());

        // A linear disasm leads with `// bn: <note>` — load-bearing, because
        // `usage::disasm_line` skips comment lines to find the instruction.
        let disasm = serde_json::json!({
            "note": "linear disassembly of 1 instruction from 0x40d214 (address-linear order, not function-bounded)",
            "text": "0040d214  e8 47 12 00 00   call    add_resource_record",
        });
        let rendered = render_disasm_linear(&disasm);
        assert!(rendered.starts_with("// bn: linear disassembly of 1 instruction"));
        assert_eq!(
            rendered.lines().nth(1),
            Some("0040d214  e8 47 12 00 00   call    add_resource_record")
        );
        // A note with no body, and a body with no note, both stay well-formed.
        assert_eq!(
            render_disasm_linear(&serde_json::json!({"note": "nothing decoded"})),
            "// bn: nothing decoded\n"
        );
        assert_eq!(
            render_disasm_linear(&serde_json::json!({"text": "0040d214  nop"})),
            "0040d214  nop\n"
        );
    }

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

