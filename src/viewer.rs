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

#[derive(Clone, Copy, PartialEq)]
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
    /// pan/scroll clears it so free panning isn't snapped back to the selection.
    follow: bool,
    /// Always-on top-left inspector for the currently highlighted block's full
    /// instructions (updates whenever `sel` changes).
    expand: CfgExpand,
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

/// Where a comment/tag lands: a concrete address (a line/hotspot address) or
/// the whole current function.
#[derive(Clone)]
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
    Comment {
        target: AnnTarget,
        buf: String,
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
}

impl Viewer {
    pub fn new(ctx: &Ctx, name: String, is_code: bool) -> Self {
        let mut viewer = Viewer {
            name,
            entry: None,
            focus_addr: None,
            view: if is_code { View::Decomp } else { View::Xrefs },
            code_view: View::Decomp,
            lines: Vec::new(),
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
        };
        viewer.load(ctx);
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
        self.load(ctx);
    }

    /// True while the viewer is capturing raw text — composing an ask, editing a
    /// rename/comment/tag, or typing an in-function search. App must not steal
    /// `?`/`^R` (and `m` is already viewer-only) while this holds.
    pub(crate) fn is_capturing_text(&self) -> bool {
        self.search_input.is_some()
            || matches!(
                self.popup,
                Popup::Ask { .. }
                    | Popup::Rename { .. }
                    | Popup::Comment { .. }
                    | Popup::Tag { .. }
            )
    }

    pub(crate) fn is_inspecting_stack(&self) -> bool {
        self.stack_view.is_open()
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
        // Leave CFG state before loading a linear view. An interior-address
        // navigation resolves to the containing function
        // via JSON and uses the address-bearing decompile to retain the exact
        // evidence location without making that address the function identity.
        self.cfg_graph_view = None;
        if self.view == View::Decomp {
            if let Some(focus) = self.focus_addr {
                let identifier = format!("0x{focus:x}");
                if let Some((name, entry, text)) = ctx.bn.decompile_json(&identifier) {
                    self.name = ctx.name_by_addr.get(&entry).cloned().unwrap_or(name);
                    self.entry = (!entry.is_empty()).then_some(entry);
                    let dec = crate::decomp::dec_lines(&text);
                    let resolved =
                        crate::decomp::resolve_stmt_addr(&crate::decomp::line_addrs(&dec), focus);
                    let line = resolved.and_then(|addr| {
                        dec.iter()
                            .position(|candidate| candidate.addr == Some(addr))
                    });
                    let plain = dec
                        .into_iter()
                        .map(|candidate| candidate.text)
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
            }
        }
        let text = match self.view {
            View::Decomp => ctx.bn.decompile(&self.name),
            View::Mlil => ctx.bn.il(&self.name, "mlil"),
            View::Disasm => ctx.bn.disasm(&self.name),
            View::Xrefs => ctx.bn.xrefs(&self.name),
            View::Cfg => unreachable!("handled above"),
        };
        self.lines = if matches!(self.view, View::Decomp) {
            syntax::tokenize_c(&text)
        } else {
            syntax::tokenize_plain(&text)
        };
        self.finish_linear_load(ctx);
        self.apply_focus();
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
                    top: usize::MAX, // sentinel: render positions the entry near the top
                    left: 0,
                    follow: true,
                    expand,
                });
                self.lines = Vec::new();
                self.spans = Vec::new();
                self.cfg_index = std::collections::HashMap::new();
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
