//! Code-viewer state and lifecycle.
//!
//! Interaction, rendering, and hotspot classification live in focused child
//! modules so this file stays the small shared model for the viewer.

mod actions;
mod hotspots;
mod input;
mod render;
mod stack;

use crate::bn::{CfgBlock, LocalVariable};
use crate::ctx::Ctx;
use crate::syntax::{self, Line};
use hotspots::build_spans;
use stack::StackView;

#[derive(Clone, Copy, PartialEq, Debug)]
enum HotKind {
    Func,  // a function symbol  -> goto / xrefs
    Data,  // a data global      -> peek / xrefs
    Addr,  // a raw 0x... inside a mapped section -> peek (or goto if code)
    Local, // a function-local variable/param -> highlight uses / type / rename
    Str,   // a string literal -> peek backing bytes / xref (resolved via bn strings)
}

/// An interactive token in the decompile: one syntax segment promoted to a
/// typed, actionable region.
struct Hotspot {
    line: usize,
    col: usize,
    target: String, // fn/data name, or the 0x-address for Addr
    kind: HotKind,
    code: bool, // Addr: inside an executable section (goto vs peek)
}

/// Which rendering of the current function/target we're showing. `i`/`I`
/// cycle the IL (the code views — also re-rendering the CFG when it's up);
/// `v` flips linear ⇄ CFG; Xrefs is entered with `x`.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Decomp,
    Mlil,
    Disasm,
    Cfg,
    Xrefs,
    /// A Binary Ninja-style linear listing of the data section around an
    /// address — entered with `g` on a data hotspot, part of the nav history.
    Data,
}

impl View {
    /// A "code" view exposes locals/stack + rename/comment/tag; the CFG and
    /// xrefs listings do not.
    fn is_code(self) -> bool {
        matches!(self, View::Decomp | View::Mlil | View::Disasm)
    }

    fn label(self) -> &'static str {
        match self {
            View::Decomp => "decompile",
            View::Mlil => "mlil",
            View::Disasm => "disasm",
            View::Cfg => "cfg",
            View::Xrefs => "xrefs",
            View::Data => "data",
        }
    }

    /// The bn rendering level backing a code view, for the CFG fetch:
    /// decompile→hlil, mlil→mlil, disasm→asm.
    fn il_level(self) -> &'static str {
        match self {
            View::Decomp => "hlil",
            View::Mlil => "mlil",
            _ => "asm",
        }
    }
}

struct Frame {
    name: String,
    view: View,
    code_view: View,
    top: usize,
    cline: usize,
    focus_addr: Option<u64>,
}

/// The laid-out 2D CFG plus its interactive state: which block is selected and
/// the canvas scroll (the graph is a pannable canvas — it may be wider/taller
/// than the pane, and the viewport follows the selection).
struct CfgGraph {
    data: crate::cfg::GraphData,
    sel: usize,
    top: usize,
    left: usize,
    /// While true the viewport tracks the selected block (after hjkl). A mouse
    /// pan/scroll or keyboard pan clears it so free panning isn't snapped back.
    follow: bool,
    /// Request from `z`: recentre the viewport on the selected block on the next
    /// render (where the viewport dimensions are known).
    recenter: bool,
    /// Top-left inspector for the highlighted block's full instructions (updates
    /// whenever `sel` changes). Toggled off with `e` so it can't cover the graph.
    expand: CfgExpand,
    expand_on: bool,
}

/// Top-left panel: the selected block's full instructions, tokenized for
/// syntax highlighting, with a scroll offset. Always present in graph mode.
struct CfgExpand {
    /// Start address of the block this panel currently shows (used to keep
    /// scroll position when the selection hasn't moved).
    addr: u64,
    title: String,
    lines: Vec<Line>,
    off: usize,
    /// Last-rendered panel bounds in screen coords `(x, y, w, h)` for mouse
    /// hit-testing (wheel scroll / ignore clicks through the panel).
    hit: Option<(u16, u16, u16, u16)>,
}

/// hjkl navigation direction across the CFG boxes.
#[derive(Clone, Copy)]
pub(crate) enum CfgDir {
    Up,
    Down,
    Left,
    Right,
}

/// What `n` is renaming: a function-local (retexted in place) or a function
/// symbol (needs a ctx rebuild so every callsite/name map picks it up).
#[derive(Clone, Copy, PartialEq)]
enum RenameScope {
    Local,
    Symbol,
}

/// Where a comment/tag lands: a concrete address (a line/hotspot address, or
/// the current function's entry for a bare `;`) or the whole current function.
#[derive(Clone, Debug, PartialEq)]
enum AnnTarget {
    Addr(String),
    Func(String),
}

impl AnnTarget {
    fn label(&self) -> String {
        match self {
            AnnTarget::Addr(a) => format!("@ {a}"),
            AnnTarget::Func(f) => format!("fn {f}"),
        }
    }
}

enum Popup {
    None,
    Ask {
        label: String,
        preview: String,
        prefix: String,
        buf: String,
    },
    Peek {
        title: String,
        lines: Vec<String>,
        /// Per-line pseudo-C tokenization aligned 1:1 with `lines`, used to
        /// syntax-colour a decompile peek. `None` for plain scrollable peeks
        /// (byte dumps, sections) which render in a flat colour.
        tokens: Option<Vec<crate::syntax::Line>>,
        /// The function to jump to with `g` — set only when the peek *is* a
        /// function's decompile (a `Func`/code-address peek). `None` for data
        /// dumps and section lists, where there's nothing to open.
        goto: Option<String>,
        off: usize,
        /// Horizontal scroll (chars) so a long line — e.g. a call whose trailing
        /// `length` argument would otherwise clip off the right edge — can be
        /// panned into view with `h`/`l`.
        hoff: usize,
        /// Absolute index of a line to highlight (the focused statement of a
        /// decomp peek); `None` for plain scrollable peeks (byte dumps, sections).
        focus: Option<usize>,
    },
    Rename {
        old: String,
        buf: String,
        err: String,
        scope: RenameScope,
    },
    Retype {
        /// Function owning the local, and the local's name (passed to bn).
        func: String,
        var: String,
        old_type: String,
        buf: String,
        /// Validation feedback: `Ok` after a passing `^P`/commit check, `Err`
        /// with the parser message after a failing one, `None` while unchecked.
        checked: Option<Result<(), String>>,
        /// Available type names (builtins + the target's declared types) for
        /// autocomplete, fetched once when the composer opens.
        types: Vec<String>,
        /// Selected autocomplete suggestion.
        sel: usize,
    },
    Comment {
        target: AnnTarget,
        buf: String,
        /// Insertion caret as a **char** index into `buf` (0..=char count), for
        /// full in-place editing (move, insert/delete mid-string).
        cursor: usize,
        /// Whether the popup opened on an *existing* comment (pre-filled `buf`).
        /// Clearing it to empty then deletes the comment; a new comment left
        /// empty is just discarded.
        existing: bool,
    },
    Tag {
        target: AnnTarget,
        buf: String,
    },
}

pub enum Exit {
    Stay,
    Back,
    /// A structural mutation (function rename) landed — the app rebuilds ctx and
    /// reloads the viewer so name maps and callsites reflect it.
    Reload,
    /// A local, non-structural mutation (comment / tag) landed — reload just this
    /// view so the annotation renders, *without* a full ctx rebuild. ctx maps
    /// don't change, and the Marks list rebuilds on its own when next opened, so
    /// the ~1s worker refresh (and its input-pausing banner) is unwarranted here.
    ReloadView,
}

pub struct Viewer {
    name: String,
    /// The entry address of the function currently in view (`0x…`), kept in sync
    /// on every successful load. Anchors identity across an *external* rename: a
    /// `^R` refresh after the agent renames this function finds the name gone,
    /// and recovers the new name by looking this address up in the rebuilt ctx.
    entry: Option<String>,
    /// Interior callsite that led to this function. Kept separate from `name`
    /// so mutations always target the containing function while IL/CFG toggles
    /// can return to the evidence site.
    focus_addr: Option<u64>,
    view: View,
    /// The IL rendering shared by linear and CFG modes — always a code view.
    /// `i`/`I` cycle it; `v` flips linear ⇄ CFG keeping it, so toggling back
    /// and forth stays on the same IL.
    code_view: View,
    lines: Vec<Line>,
    /// Per-line instruction address for the current linear code listing, aligned
    /// to `lines` by index (`None` on a line with no address of its own — a
    /// declaration, brace, or blank separator). Cached at load so the header can
    /// show where the cursor is without re-decompiling every frame; empty for
    /// non-code views.
    ///
    /// INVARIANT — index alignment with `lines` (load-bearing; see `load`,
    /// [`decomp_view_lines`] and `line_addr_column`): entry `i` describes
    /// `lines[i]`. Both sides now come from **one** backend read per load — the
    /// decompile view from a single `--addresses` payload, MLIL/disasm from the
    /// one flattened listing — so alignment is by construction rather than by
    /// reconciliation. It may still be *shorter* than `lines` (an empty column on
    /// a view that has no addresses at all), so it is never longer. Consumers
    /// index defensively (`.get`, clamp) and must never assume
    /// `code_addrs.len() == lines.len()`.
    code_addrs: Vec<Option<u64>>,
    spans: Vec<Hotspot>,
    locals: std::collections::HashMap<String, String>, // name -> type (this fn)
    stack_view: StackView,
    active: Option<usize>, // Tab/click-selected span (else derived from cline)
    top: usize,
    cline: usize,
    status: String,
    vmode: bool,
    vanchor: usize,
    search: String,               // committed query (for ]/[)
    search_input: Option<String>, // Some while typing after `/`
    goto_input: Option<String>,   // Some while typing an address/symbol after `:`
    /// Vim `g`-prefix latch: set by a first `g`, consumed by the next key —
    /// `gg` jumps to the top. Reset on every other key, so an abandoned prefix
    /// never lingers.
    g_pending: bool,
    stack: Vec<Frame>,
    /// Views popped by `b`, so `w` can walk forward again. Cleared whenever a
    /// *new* navigation (goto/xrefs) branches off the history.
    forward: Vec<Frame>,
    popup: Popup,
    screen_tgts: Vec<(u16, u16, u16, usize)>, // x0,x1,y,target_idx (for mouse)
    /// CFG view: block identity *and* displayed head address -> its header
    /// line, so acting on an edge target or a block header jumps within the
    /// graph instead of re-decompiling. Empty elsewhere.
    cfg_index: std::collections::HashMap<u64, usize>,
    /// CFG view: whether the boxed graph layout is requested (Space toggles it).
    cfg_graph: bool,
    /// CFG view: the fetched basic blocks for the named function, so layout
    /// changes (graph↔list toggle, Enter-read, toggling back to CFG) re-run
    /// only the pure render instead of the external `bn cfg` query. Invalidated
    /// by a reload (^R / rename), a function change, or an IL change (the key
    /// is name + IL level).
    cfg_cache: Option<(String, &'static str, Vec<CfgBlock>)>,
    /// CFG graph mode: the laid-out graph + selection, when a 2D graph is shown
    /// (None while in the CFG list fallback or any non-CFG view).
    cfg_graph_view: Option<CfgGraph>,
    /// CFG graph mode: screen rects of each block box for click hit-testing:
    /// (x0, x1, y0, y1, block index).
    cfg_hit: Vec<(u16, u16, u16, u16, usize)>,
    /// CFG graph mode: last mouse position while a button is held (for drag-pan),
    /// and whether this press has moved (so a click still selects, a drag pans).
    cfg_drag: Option<(u16, u16)>,
    cfg_dragged: bool,
    /// Content width the comment composer last wrapped at, stashed by the (`&self`)
    /// popup renderer so `Up`/`Down` in the editor can move the caret by one
    /// visual row. `Cell` for interior mutability during render.
    comment_wrap: std::cell::Cell<usize>,
}

impl Viewer {
    pub fn new(ctx: &Ctx, name: String, is_code: bool) -> Self {
        let mut viewer = Viewer::blank(name, is_code);
        viewer.load(ctx);
        viewer
    }

    /// The viewer's initial state, before its first fetch. Split out of [`new`]
    /// so the load-independent state machine (position keeping, target
    /// resolution) is reachable from unit tests without a live `Ctx`.
    fn blank(name: String, is_code: bool) -> Self {
        Viewer {
            name,
            entry: None,
            focus_addr: None,
            view: if is_code { View::Decomp } else { View::Xrefs },
            code_view: View::Decomp,
            lines: Vec::new(),
            code_addrs: Vec::new(),
            spans: Vec::new(),
            locals: std::collections::HashMap::new(),
            stack_view: StackView::default(),
            active: None,
            top: 0,
            cline: 0,
            status: String::new(),
            vmode: false,
            vanchor: 0,
            search: String::new(),
            search_input: None,
            goto_input: None,
            g_pending: false,
            stack: Vec::new(),
            forward: Vec::new(),
            popup: Popup::None,
            screen_tgts: Vec::new(),
            cfg_index: std::collections::HashMap::new(),
            cfg_graph: true,
            cfg_cache: None,
            cfg_graph_view: None,
            cfg_hit: Vec::new(),
            cfg_drag: None,
            cfg_dragged: false,
            comment_wrap: std::cell::Cell::new(60),
        }
    }

    /// A viewer sitting in `view` with nothing loaded — lets the submodules' unit
    /// tests drive the pure state machine (target resolution, position keeping)
    /// with hand-built `lines`/`code_addrs` and no backend at all.
    #[cfg(test)]
    fn test_blank(name: String, view: View) -> Self {
        let mut viewer = Viewer::blank(name, true);
        viewer.view = view;
        viewer.code_view = view;
        viewer
    }

    /// Re-fetch the current view against a (possibly rebuilt) ctx — used after a
    /// function rename or a manual refresh so renamed symbols/callsites and any
    /// new comments render. Keeps the cursor line; drops the transient selection
    /// and the CFG block cache (the function may have changed under us).
    pub fn reload(&mut self, ctx: &Ctx) {
        self.cfg_cache = None;
        // Self-heal an external rename: if the current display name no longer
        // resolves in the rebuilt ctx but we know this function's entry address,
        // adopt whatever name now lives at that address. Without this, a `^R`
        // after the agent renames the viewed function keeps the stale name and
        // the next fetch returns nothing. Only for a *name*-anchored view — when
        // `self.name` is a bare address (goto/xrefs onto a `0x…` hotspot) the
        // view is already address-anchored and `entry` may point at a different
        // function, so recovery there would teleport the view.
        if !self.name.starts_with("0x") && !ctx.addr_by_name.contains_key(&self.name) {
            if let Some(name) = self.entry.as_ref().and_then(|a| ctx.name_by_addr.get(a)) {
                if *name != self.name {
                    self.status = format!(" ↻ renamed externally: {} → {name}", self.name);
                    self.name = name.clone();
                }
            }
        }
        // Every view recentres on its focus address as it builds, which is right
        // on first entry and wrong on a *reload*: `reload` means "re-fetch what
        // I'm looking at", and `focus_addr` still holds the address of whatever
        // navigation first opened this function. Preserving the position keeps a
        // comment/tag/retype redraw (`Exit::ReloadView`) — and a `^R` refresh —
        // on the line being annotated instead of throwing the reader back to the
        // goto's landing address (issue #23). Explicit navigation re-centres by
        // calling `load` directly, not through here.
        let keep = self.keep_position();
        self.load(ctx);
        self.restore_position(keep);
    }

    /// The cursor/scroll pair to carry across a reload.
    fn keep_position(&self) -> (usize, usize) {
        (self.cline, self.top)
    }

    /// Put the cursor and scroll back after a reload, clamped to the rebuilt
    /// listing (the function may have shrunk under us).
    fn restore_position(&mut self, (cline, top): (usize, usize)) {
        let max = self.lines.len().saturating_sub(1);
        self.cline = cline.min(max);
        self.top = top.min(self.cline);
    }

    /// True while the viewer is capturing raw text — composing an ask, editing a
    /// rename/comment/tag, or typing an in-function search. App must not steal
    /// `?`/`^R` (and `m` is already viewer-only) while this holds.
    pub(crate) fn is_capturing_text(&self) -> bool {
        self.search_input.is_some()
            || self.goto_input.is_some()
            || matches!(
                self.popup,
                Popup::Ask { .. }
                    | Popup::Rename { .. }
                    | Popup::Retype { .. }
                    | Popup::Comment { .. }
                    | Popup::Tag { .. }
            )
    }

    pub(crate) fn is_inspecting_stack(&self) -> bool {
        self.stack_view.is_open()
    }

    /// Show `text` as the entire code pane, with no address column behind it and
    /// **no second backend read**. Used when the one decompile read came back empty
    /// or failed: the reader sees that answer, not a retry's.
    fn show_note(&mut self, ctx: &Ctx, text: String) {
        self.lines = syntax::tokenize_plain(&text);
        self.code_addrs = Vec::new();
        self.finish_linear_load(ctx);
        self.apply_focus();
    }

    fn load(&mut self, ctx: &Ctx) {
        // Keep the entry-address anchor current whenever the name resolves to a
        // known symbol (see `entry`), so a later reload can recover from a rename.
        if let Some(addr) = ctx.addr_by_name.get(&self.name) {
            self.entry = Some(addr.clone());
        }
        if matches!(self.view, View::Cfg) {
            self.load_cfg(ctx);
            self.apply_focus();
            return;
        }
        if matches!(self.view, View::Data) {
            self.load_data(ctx);
            return;
        }
        // Rebuilt below for the code views; empty means "no address readout".
        self.code_addrs = Vec::new();
        // Leave CFG state before loading a linear view.
        self.cfg_graph_view = None;
        // The decompile view is fetched **once**, with `--addresses`, whether it
        // was reached by name or by an interior address: `decomp_view_lines`
        // derives the rendered lines and the address column from that one payload,
        // so the `code_addrs` alignment invariant holds by construction instead of
        // reconciling two independent backend reads (issue #18). An interior
        // address additionally resolves to its containing function, which becomes
        // the view's identity while `focus_addr` keeps the evidence site.
        if self.view == View::Decomp {
            let focus = self.focus_addr;
            let identifier = match focus {
                Some(focus) => format!("0x{focus:x}"),
                None => self.name.clone(),
            };
            // Whatever that one read said is what the reader sees. Re-reading on a
            // failure would pay a timed-out backend's budget twice and could resolve
            // against different live state — and for an interior focus it would ask
            // for `self.name` instead of the address that was requested.
            let read = match ctx.bn.decompile_read(&identifier) {
                Ok(Some(read)) => read,
                Ok(None) => {
                    self.show_note(ctx, format!("(no decompilation for {identifier})"));
                    return;
                }
                Err(error) => {
                    self.show_note(ctx, format!("✗ {error}"));
                    return;
                }
            };
            if focus.is_some() {
                self.name = ctx
                    .name_by_addr
                    .get(&read.entry)
                    .cloned()
                    .unwrap_or(read.name);
                self.entry = (!read.entry.is_empty()).then_some(read.entry);
            }
            let dec = decomp_view_lines(&read.text, read.note.as_deref(), &read.warnings);
            let line = focus.and_then(|focus| {
                let resolved =
                    crate::decomp::resolve_stmt_addr(&crate::decomp::line_addrs(&dec), focus);
                resolved.and_then(|addr| {
                    dec.iter()
                        .position(|candidate| candidate.addr == Some(addr))
                })
            });
            self.code_addrs = dec.iter().map(|candidate| candidate.addr).collect();
            let plain = dec
                .iter()
                .map(|candidate| candidate.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            self.lines = syntax::tokenize_c(&plain);
            self.finish_linear_load(ctx);
            if let Some(line) = line {
                self.cline = line;
                self.top = line.saturating_sub(3);
            }
            return;
        }
        let text = match self.view {
            View::Mlil => self.linear_annotated(ctx, View::Mlil.il_level()),
            View::Disasm => self.linear_annotated(ctx, View::Disasm.il_level()),
            View::Xrefs => ctx.bn.xrefs(&self.name),
            // Decomp returns above, out of its single `--addresses` read; there is
            // deliberately no second text-mode decompile to fall back to.
            View::Decomp | View::Cfg | View::Data => unreachable!("handled above"),
        };
        self.lines = syntax::tokenize_plain(&text);
        self.code_addrs = self.line_addr_column(&text);
        // Guards the `code_addrs`/`lines` alignment invariant (never longer than
        // the lines it indexes). Debug-only: a violation is a silent-wrong-address
        // bug, not a crash, so we catch it in dev rather than defend in release.
        debug_assert!(
            self.code_addrs.len() <= self.lines.len(),
            "code_addrs ({}) longer than lines ({}) in {} — alignment broken",
            self.code_addrs.len(),
            self.lines.len(),
            self.view.label(),
        );
        self.finish_linear_load(ctx);
        self.apply_focus();
    }

    /// Per-line addresses aligned to `self.lines`, for the header's cursor-address
    /// readout. MLIL/disasm print an 8-hex address column, so it's read straight
    /// off the same flattened listing that produced the lines — aligned by
    /// construction.
    ///
    /// The decompile view does **not** come through here: it builds its lines and
    /// its address column together in `load`, out of one `--addresses` read (see
    /// [`decomp_view_lines`]). It used to be reconciled here from a *second*
    /// decompile, which is what made the alignment invariant span two non-atomic
    /// reads on a live instance (issue #18).
    fn line_addr_column(&self, linear_text: &str) -> Vec<Option<u64>> {
        match self.view {
            View::Mlil | View::Disasm => linear_text
                .lines()
                .map(|line| {
                    let bytes = line.as_bytes();
                    (bytes.len() >= 8 && bytes[..8].iter().all(u8::is_ascii_hexdigit))
                        .then(|| u64::from_str_radix(&line[..8], 16).ok())
                        .flatten()
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The instruction address the cursor is on, for the header readout: the
    /// current line's own address, else the nearest addressed line at/below it,
    /// else above (so a brace or blank line still reports a sensible address).
    /// `None` outside a linear code view.
    pub(super) fn cursor_addr(&self) -> Option<u64> {
        if self.code_addrs.is_empty() {
            return None;
        }
        let clamp = self.cline.min(self.code_addrs.len().saturating_sub(1));
        self.code_addrs
            .get(clamp)
            .copied()
            .flatten()
            .or_else(|| self.code_addrs[clamp..].iter().find_map(|addr| *addr))
            .or_else(|| self.code_addrs[..clamp].iter().rev().find_map(|addr| *addr))
    }

    /// The annotated linear listing for the MLIL/disasm views: flatten the same
    /// `bb.disassembly_text` blocks the CFG uses (`bn.cfg`) rather than the
    /// plain-text `bn il`/`bn disasm`, so call targets, data references, and
    /// stack slots are symbolized instead of bare addresses — and so a resolved
    /// call renders as a `Func` hotspot, distinct from a local branch label. The
    /// CFG block cache is shared: when it already holds this function at `il`,
    /// the fetch is skipped (a linear↔CFG toggle at the same level is free).
    fn linear_annotated(&mut self, ctx: &Ctx, il: &'static str) -> String {
        let blocks = match self.cfg_cache.take() {
            Some((name, cached_il, blocks)) if name == self.name && cached_il == il => blocks,
            _ => ctx.bn.cfg(&self.name, il),
        };
        let text = crate::cfg::flat(&blocks);
        self.cfg_cache = Some((self.name.clone(), il, blocks));
        if text.trim().is_empty() {
            match il {
                "mlil" => "(no IL)".into(),
                _ => "(no disassembly)".into(),
            }
        } else {
            text
        }
    }

    fn finish_linear_load(&mut self, ctx: &Ctx) {
        // Stack layout is useful in every code view; local-name hotspots remain
        // limited to decompile/MLIL where BN actually renders those names.
        let local_variables = if self.view.is_code() {
            ctx.bn.local_list(&self.name)
        } else {
            Vec::new()
        };
        self.stack_view.set_locals(&local_variables);
        self.locals = if matches!(self.view, View::Decomp | View::Mlil) {
            local_type_map(&local_variables)
        } else {
            std::collections::HashMap::new()
        };
        self.spans = build_spans(&self.lines, ctx, &self.locals);
        self.active = None;
    }

    /// Re-center non-decompile views on the interior site retained by a goto.
    /// Decompile is handled from its address-bearing JSON in `load` above.
    fn apply_focus(&mut self) {
        let Some(focus) = self.focus_addr else { return };
        if self.view == View::Cfg {
            let Some((index, key)) = self.cfg_cache.as_ref().and_then(|(_, _, blocks)| {
                blocks
                    .iter()
                    .enumerate()
                    .filter_map(|(index, block)| {
                        let nearest = block
                            .insns
                            .iter()
                            .filter_map(|insn| crate::ctx::parse_hex(&insn.a))
                            .min_by_key(|addr| addr.abs_diff(focus))?;
                        Some((nearest.abs_diff(focus), index, block.start.clone()))
                    })
                    .min_by_key(|(distance, _, _)| *distance)
                    .map(|(_, index, key)| (index, key))
            }) else {
                return;
            };
            if self.cfg_graph_view.is_some() {
                self.cfg_select(index);
            } else if let Some(line) =
                crate::ctx::parse_hex(&key).and_then(|addr| self.cfg_index.get(&addr).copied())
            {
                self.cline = line;
                self.top = line.saturating_sub(3);
            }
            return;
        }
        if self.view == View::Decomp || self.lines.is_empty() {
            return;
        }
        let best = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(line, segments)| {
                let token = segments.iter().find_map(|segment| {
                    let text = segment.text.trim();
                    let normalized = text.strip_prefix("0x").unwrap_or(text);
                    (normalized.len() >= 6 && normalized.chars().all(|ch| ch.is_ascii_hexdigit()))
                        .then(|| u64::from_str_radix(normalized, 16).ok())
                        .flatten()
                })?;
                Some((token.abs_diff(focus), line))
            })
            .min_by_key(|(distance, _)| *distance)
            .map(|(_, line)| line);
        if let Some(line) = best {
            self.cline = line;
            self.top = line.saturating_sub(3);
        }
    }

    /// Build the CFG view at the current IL (`code_view`): walk the function's
    /// basic blocks via `bn` (reusing the cached fetch when it's for the same
    /// function *and* IL — layout toggles and re-entries must not re-spawn the
    /// external query), render them (list or boxed graph) into display lines,
    /// and record the block-address → line index so acting on an edge target
    /// jumps within the graph.
    fn load_cfg(&mut self, ctx: &Ctx) {
        let il = self.code_view.il_level();
        let blocks = match self.cfg_cache.take() {
            Some((name, cached_il, blocks)) if name == self.name && cached_il == il => blocks,
            _ => ctx.bn.cfg(&self.name, il),
        };
        // Try the 2D graph first (when requested); fall back to the block list
        // only when there are too many blocks to lay out.
        if self.cfg_graph {
            if let Some(data) = crate::cfg::graph(&blocks) {
                // Preserve the selection across a re-layout (e.g. refresh) by address.
                let keep = self
                    .cfg_graph_view
                    .as_ref()
                    .and_then(|g| g.data.blocks.get(g.sel).map(|b| b.addr));
                let sel = keep
                    .and_then(|a| data.blocks.iter().position(|b| b.addr == a))
                    .unwrap_or(data.entry);
                let count = data.block_count;
                let expand = Self::build_cfg_expand(&data, sel);
                self.cfg_graph_view = Some(CfgGraph {
                    data,
                    sel,
                    top: usize::MAX, // sentinel: render centres the graph on first draw
                    left: 0,
                    follow: true,
                    recenter: false,
                    expand,
                    expand_on: true,
                });
                self.lines = Vec::new();
                self.spans = Vec::new();
                self.cfg_index = std::collections::HashMap::new();
                // Locals for the inspector's hotspot layer — the same map the
                // linear views build — so a local reads grey there too. Calls,
                // data, and addresses need no locals (they resolve via ctx), but
                // parity with linear means carrying the local names as well.
                let locals = ctx.bn.local_list(&self.name);
                self.locals = if matches!(self.code_view, View::Decomp | View::Mlil) {
                    local_type_map(&locals)
                } else {
                    std::collections::HashMap::new()
                };
                self.status = format!(
                    " cfg·{} · {count} blocks · graph · hjkl spatial · ]/[ block · Space list · i il · v linear",
                    self.code_view.label()
                );
                self.cfg_cache = Some((self.name.clone(), il, blocks));
                return;
            }
        }
        // List fallback (too many blocks) / explicit toggle.
        self.cfg_graph_view = None;
        let rendered = crate::cfg::list(&blocks);
        let text = rendered.lines.join("\n");
        self.lines = syntax::tokenize_plain(&text);
        self.cfg_index = rendered.index;
        self.stack_view.set_locals(&[]);
        self.locals = std::collections::HashMap::new();
        self.spans = build_spans(&self.lines, ctx, &self.locals);
        self.active = None;
        self.status = if self.cfg_graph {
            format!(
                " cfg·{} · {} blocks · list (too many to graph) · Enter jump edge · i il · v linear",
                self.code_view.label(),
                rendered.block_count
            )
        } else {
            format!(
                " cfg·{} · {} blocks · list · Space graph · Enter jump edge · i il · v linear",
                self.code_view.label(),
                rendered.block_count
            )
        };
        self.cfg_cache = Some((self.name.clone(), il, blocks));
    }

    /// Build the linear data view: resolve the section around `focus_addr`
    /// (or `self.name`), fetch its typed data vars + raw bytes, and render the
    /// Binary Ninja-style listing. Names/strings/pointer targets become hotspots
    /// via `build_spans`, so you can step, peek, xref, or follow them.
    fn load_data(&mut self, ctx: &Ctx) {
        self.cfg_graph_view = None;
        let addr = self
            .focus_addr
            .or_else(|| crate::ctx::parse_hex(&self.name));
        let Some(addr) = addr else {
            self.lines = Vec::new();
            self.spans = Vec::new();
            self.status = " ✗ no address for the data view".into();
            return;
        };
        // Window: the whole section when small, else a bounded slice centred on
        // the address (a huge .rodata mustn't be read/rendered in full).
        const CAP: u64 = 0x2000;
        let (lo, hi, label) = match ctx.section_of(addr) {
            Some((start, end, name, _)) => {
                let (lo, hi) = if end - start <= CAP {
                    (*start, *end)
                } else {
                    let lo = addr.saturating_sub(CAP / 3).max(*start);
                    (lo, (lo + CAP).min(*end))
                };
                (lo, hi, format!("{name}  0x{start:x}–0x{end:x}"))
            }
            None => (
                addr.saturating_sub(0x40),
                addr + 0x200,
                "(no section)".to_string(),
            ),
        };
        let lo_str = format!("0x{lo:x}");
        let hi_str = format!("0x{hi:x}");
        let vars = ctx.bn.data_vars(&lo_str, &hi_str);
        let dump = ctx.bn.read_dump(&lo_str, (hi - lo) as usize);
        let bytes = crate::datamap::parse_hexdump(&dump, lo);
        let result = crate::datamap::linear(&label, lo, hi, &vars, &bytes, addr);
        self.lines = result.lines;
        self.locals = std::collections::HashMap::new();
        self.stack_view.set_locals(&[]);
        self.spans = build_spans(&self.lines, ctx, &self.locals);
        self.active = None;
        let line = result.focus.unwrap_or(0);
        self.cline = line;
        self.top = line.saturating_sub(3);
    }

    /// Move the CFG graph selection to the nearest box in `dir` (hjkl — spatial).
    pub(crate) fn cfg_move(&mut self, dir: CfgDir) {
        let Some(g) = self.cfg_graph_view.as_mut() else {
            return;
        };
        let blocks = &g.data.blocks;
        if blocks.len() < 2 {
            return;
        }
        let (ccx, ccy) = (blocks[g.sel].cx() as i64, blocks[g.sel].cy() as i64);
        let mut best: Option<(i64, usize)> = None;
        for (i, b) in blocks.iter().enumerate() {
            if i == g.sel {
                continue;
            }
            let (dx, dy) = (b.cx() as i64 - ccx, b.cy() as i64 - ccy);
            let in_dir = match dir {
                CfgDir::Up => dy < 0,
                CfgDir::Down => dy > 0,
                CfgDir::Left => dx < 0,
                CfgDir::Right => dx > 0,
            };
            if !in_dir {
                continue;
            }
            // Favour the dominant axis; break ties by the perpendicular offset.
            let score = match dir {
                CfgDir::Up | CfgDir::Down => dy.abs() * 2 + dx.abs(),
                CfgDir::Left | CfgDir::Right => dx.abs() * 2 + dy.abs(),
            };
            if best.map_or(true, |(s, _)| score < s) {
                best = Some((score, i));
            }
        }
        if let Some((_, i)) = best {
            g.sel = i;
            g.follow = true; // re-center the viewport on the new selection
            Self::sync_cfg_expand(g);
        }
    }

    /// Step the CFG selection by block index order (`n`/`N` — sequential, not
    /// spatial). Wraps at the ends so you can walk every block once.
    pub(crate) fn cfg_step(&mut self, delta: i64) {
        let Some(g) = self.cfg_graph_view.as_mut() else {
            return;
        };
        let n = g.data.blocks.len() as i64;
        if n == 0 {
            return;
        }
        let next = (g.sel as i64 + delta).rem_euclid(n) as usize;
        g.sel = next;
        g.follow = true;
        Self::sync_cfg_expand(g);
    }

    /// Select CFG graph block `idx` (from a mouse click) and refresh the
    /// top-left inspector to match.
    pub(crate) fn cfg_select(&mut self, idx: usize) {
        let Some(g) = self.cfg_graph_view.as_mut() else {
            return;
        };
        if idx >= g.data.blocks.len() {
            return;
        }
        g.sel = idx;
        Self::sync_cfg_expand(g);
    }

    /// Pan the CFG canvas by `(dx, dy)` cells (`HJKL` — free keyboard pan).
    /// Clears follow so the canvas stays where you put it; the render clamps it
    /// to the padded virtual canvas so a pan can't lose the graph. A no-op before
    /// the first render has placed the graph (`top` still the sentinel).
    pub(crate) fn cfg_pan(&mut self, dx: i64, dy: i64) {
        let Some(g) = self.cfg_graph_view.as_mut() else {
            return;
        };
        if g.top == usize::MAX {
            return;
        }
        g.follow = false;
        g.top = (g.top as i64 + dy).max(0) as usize;
        g.left = (g.left as i64 + dx).max(0) as usize;
    }

    /// Recentre the viewport on the selected block (`z`). Deferred to the render,
    /// which knows the viewport size, via the `recenter` flag.
    pub(crate) fn cfg_recenter(&mut self) {
        if let Some(g) = self.cfg_graph_view.as_mut() {
            g.recenter = true;
            g.follow = true;
        }
    }

    /// Toggle the top-left block inspector (`e`). Off gives the graph the whole
    /// canvas; on shows the highlighted block's full instructions.
    pub(crate) fn cfg_toggle_expand(&mut self) {
        if let Some(g) = self.cfg_graph_view.as_mut() {
            g.expand_on = !g.expand_on;
        }
    }

    /// Enter/`g` in the CFG graph: drop into the block list scrolled to the
    /// selected block so its instructions can be read.
    pub(crate) fn cfg_read_selected(&mut self, ctx: &Ctx) {
        let addr = self
            .cfg_graph_view
            .as_ref()
            .and_then(|g| g.data.blocks.get(g.sel).map(|b| b.addr));
        self.cfg_graph = false;
        self.load(ctx);
        if let Some(line) = addr.and_then(|a| self.cfg_index.get(&a).copied()) {
            self.cline = line;
            self.top = line.saturating_sub(2);
        }
    }

    pub(crate) fn in_cfg_graph(&self) -> bool {
        self.cfg_graph_view.is_some()
    }

    /// Build the always-on inspector for `data.blocks[sel]`.
    fn build_cfg_expand(data: &crate::cfg::GraphData, sel: usize) -> CfgExpand {
        let (addr, title, text) = match data.blocks.get(sel) {
            Some(b) if !b.insns.is_empty() => {
                let title = format!("{}  {:#x}", b.label, b.head);
                let text = b
                    .insns
                    .iter()
                    .map(|(a, t)| format!("{a}  {t}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                (b.addr, title, text)
            }
            Some(b) => (
                b.addr,
                format!("{}  {:#x}", b.label, b.head),
                "(no instructions)".into(),
            ),
            None => (0, "block".into(), "(no block)".into()),
        };
        CfgExpand {
            addr,
            title,
            lines: syntax::tokenize_plain(&text),
            off: 0,
            hit: None,
        }
    }

    /// Refresh the top-left inspector for the current selection. Keeps the
    /// scroll offset when the same block is still selected; resets it on change.
    fn sync_cfg_expand(g: &mut CfgGraph) {
        let prev_addr = g.expand.addr;
        let prev_off = g.expand.off;
        let mut exp = Self::build_cfg_expand(&g.data, g.sel);
        if exp.addr == prev_addr {
            let max = exp.lines.len().saturating_sub(1);
            exp.off = prev_off.min(max);
        }
        g.expand = exp;
    }

    /// Scroll the always-on expand panel. Returns true if it consumed the key.
    pub(crate) fn cfg_expand_scroll(&mut self, delta: i64) -> bool {
        let Some(g) = self.cfg_graph_view.as_mut() else {
            return false;
        };
        let exp = &mut g.expand;
        let max = exp.lines.len().saturating_sub(1);
        exp.off = (exp.off as i64 + delta).clamp(0, max as i64) as usize;
        true
    }

    /// True when `(col, row)` falls inside the last-rendered expand panel.
    pub(crate) fn cfg_expand_hit(&self, col: u16, row: u16) -> bool {
        self.cfg_graph_view
            .as_ref()
            .and_then(|g| g.expand.hit)
            .is_some_and(|(x, y, w, h)| col >= x && col < x + w && row >= y && row < y + h)
    }

    /// The effective selected span: the Tab/click-selected one if it's still on
    /// the cursor line, else the first span on the cursor line.
    fn cur_span(&self) -> Option<usize> {
        if let Some(i) = self.active {
            if self.spans.get(i).is_some_and(|s| s.line == self.cline) {
                return Some(i);
            }
        }
        self.spans.iter().position(|s| s.line == self.cline)
    }

    fn line_text(&self, line: usize) -> String {
        self.lines
            .get(line)
            .map(|segments| {
                segments
                    .iter()
                    .map(|segment| segment.text.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn local_type_map(locals: &[LocalVariable]) -> std::collections::HashMap<String, String> {
    locals
        .iter()
        .map(|local| (local.name.clone(), local.type_name.clone()))
        .collect()
}

/// Assemble the decompile view from **one** `bn decompile --addresses` read: the
/// rendered line and its own address, together, per entry.
///
/// The three pieces the CLI's text mode shows are laid out in its order — the
/// `// bn:` interior-resolution note first, then the pseudo-C body with its
/// address column stripped ([`crate::decomp::dec_lines`]), then a blank separator
/// and the `warning: …` lines. Note and warnings are bn's own commentary, not
/// statements, so they carry no address (`addr: None`) — which is precisely why
/// they must be added *around* `dec_lines` rather than fed through it: the note is
/// a prepend, and a prepend applied to only one of the two sides is exactly the
/// silent off-by-one that made the old two-read path fragile (issue #18).
///
/// The result is the single source for both `lines` and `code_addrs`, so the
/// alignment invariant on [`Viewer::code_addrs`] holds by construction.
fn decomp_view_lines(
    addr_text: &str,
    note: Option<&str>,
    warnings: &[String],
) -> Vec<crate::decomp::DecLine> {
    use crate::decomp::DecLine;
    let body = crate::decomp::dec_lines(addr_text);
    let mut rows = Vec::with_capacity(body.len() + warnings.len() + 2);
    if let Some(note) = note {
        rows.push(DecLine {
            addr: None,
            text: note.to_string(),
        });
    }
    rows.extend(body);
    if !warnings.is_empty() {
        rows.push(DecLine {
            addr: None,
            text: String::new(),
        });
        rows.extend(warnings.iter().map(|warning| DecLine {
            addr: None,
            text: warning.clone(),
        }));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{decomp_view_lines, View, Viewer};

    // A mock `bn decompile --addresses` body: 8-hex address column, an 8-space
    // base gap, +4 per nesting level. Matches the shape `dec_lines` parses.
    const DEC: &str = "\
00401200        int32_t parse_hdr(void* buf, size_t len)
00401200        {
00401214            uint32_t magic = *(uint32_t*)buf;

00401220            if (magic != 0x4d475a)
00401228                return -1;
00401234            memcpy(&hdr, buf, len);
00401200        }";

    /// #18: one payload in, aligned (lines, addresses) out — including the two
    /// address-less shapes the old two-read path only survived by luck.
    #[test]
    fn one_addresses_payload_yields_aligned_lines_and_addrs() {
        let warnings = vec!["warning: analysis stub — body may be incomplete".to_string()];
        let note = "// bn: 0x401228 is inside parse_hdr @ 0x401200 (+0x28); showing the containing function";
        let dec = decomp_view_lines(DEC, Some(note), &warnings);

        // The note leads, address-less, and does not de-indent the body with it.
        assert_eq!(dec[0].addr, None);
        assert!(dec[0].text.starts_with("// bn:"));
        assert_eq!(dec[1].text, "int32_t parse_hdr(void* buf, size_t len)");
        assert_eq!(dec[1].addr, Some(0x401200));
        assert_eq!(dec[6].text, "        return -1;");
        assert_eq!(dec[6].addr, Some(0x401228));

        // The warning block trails, address-less, after a blank separator.
        assert_eq!(
            dec[dec.len() - 2],
            crate::decomp::DecLine {
                addr: None,
                text: String::new()
            }
        );
        assert_eq!(dec.last().unwrap().addr, None);
        assert_eq!(dec.last().unwrap().text, warnings[0]);

        // The invariant: rendering and indexing the SAME list keeps every line's
        // address on that line. `tokenize_c` splits on '\n' 1:1, so the two are
        // equal length — not merely "addrs no longer than lines".
        let plain = dec
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let lines = crate::syntax::tokenize_c(&plain);
        let addrs: Vec<Option<u64>> = dec.iter().map(|line| line.addr).collect();
        assert_eq!(addrs.len(), lines.len());
        // Spot-check the alignment end to end: the `memcpy` statement's address
        // is the entry at the index that renders it.
        let memcpy = lines
            .iter()
            .position(|segments| segments.iter().any(|s| s.text == "memcpy"))
            .expect("the mock body has a memcpy line");
        assert_eq!(addrs[memcpy], Some(0x401234));

        // Sentinel for the shape that used to break: the old path rendered the
        // note-prefixed text from one read and took its address column from a
        // *separate* `dec_lines`, which drops the note — one line of silent
        // offset, so every line reported the address of the statement *below* it.
        // A `;` on `return -1;` would then have commented the memcpy.
        let unnoted: Vec<Option<u64>> = crate::decomp::dec_lines(DEC)
            .iter()
            .map(|line| line.addr)
            .collect();
        let ret = memcpy - 1;
        assert_eq!(addrs[ret], Some(0x401228));
        assert_eq!(unnoted[ret], Some(0x401234));
    }

    /// The common case — a by-name read with nothing to annotate — must not gain
    /// a leading or trailing line.
    #[test]
    fn a_bare_payload_gains_no_extra_lines() {
        let dec = decomp_view_lines(DEC, None, &[]);
        assert_eq!(dec.len(), DEC.lines().count());
        assert_eq!(dec[0].addr, Some(0x401200));
        assert_eq!(dec.last().unwrap().text, "}");
    }

    /// #23: a reload (comment/tag/retype redraw, `^R`) keeps the reading position
    /// in a code view — it used to be preserved only for `View::Data`, so every
    /// annotation snapped the cursor back to the goto's landing address.
    #[test]
    fn reload_keeps_the_cursor_position_in_a_code_view() {
        let mut viewer = Viewer::blank("parse_hdr".into(), true);
        viewer.view = View::Decomp;
        viewer.lines = vec![Vec::new(); 82];
        viewer.cline = 64;
        viewer.top = 61;
        let keep = viewer.keep_position();
        assert_eq!(keep, (64, 61));

        // A shorter listing after the reload clamps instead of pointing past the end.
        viewer.lines = vec![Vec::new(); 12];
        viewer.restore_position(keep);
        assert_eq!(viewer.cline, 11);
        assert_eq!(viewer.top, 11);
    }
}
