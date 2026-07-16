//! Code-viewer state and lifecycle.
//!
//! Interaction, rendering, and hotspot classification live in focused child
//! modules so this file stays the small shared model for the viewer.

mod actions;
mod hotspots;
mod input;
mod render;
mod stack;

use crate::bn::LocalVariable;
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

/// Which rendering of the current function/target we're showing. The code
/// views cycle with `i`/`I`/`v`; Xrefs is entered with `x`.
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
}

struct Frame {
    name: String,
    view: View,
    top: usize,
    cline: usize,
}

/// What `r` is renaming: a function-local (retexted in place) or a function
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
    view: View,
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
    search: String,               // committed query (for n/N)
    search_input: Option<String>, // Some while typing after `/`
    stack: Vec<Frame>,
    popup: Popup,
    screen_tgts: Vec<(u16, u16, u16, usize)>, // x0,x1,y,target_idx (for mouse)
    /// CFG view: block start address -> its header line, so acting on an edge
    /// target jumps within the graph instead of re-decompiling. Empty elsewhere.
    cfg_index: std::collections::HashMap<u64, usize>,
    /// CFG view: whether the boxed graph layout is requested (Space toggles it).
    cfg_graph: bool,
}

impl Viewer {
    pub fn new(ctx: &Ctx, name: String, is_code: bool) -> Self {
        let mut viewer = Viewer {
            name,
            view: if is_code { View::Decomp } else { View::Xrefs },
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
            popup: Popup::None,
            screen_tgts: Vec::new(),
            cfg_index: std::collections::HashMap::new(),
            cfg_graph: false,
        };
        viewer.load(ctx);
        viewer
    }

    /// Re-fetch the current view against a (possibly rebuilt) ctx — used after a
    /// function rename or a manual refresh so renamed symbols/callsites and any
    /// new comments render. Keeps the cursor line; drops the transient selection.
    pub fn reload(&mut self, ctx: &Ctx) {
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
        if matches!(self.view, View::Cfg) {
            self.load_cfg(ctx);
            return;
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

    /// Build the CFG view: walk the function's basic blocks via `bn`, render them
    /// (list or boxed graph) into display lines, and record the block-address →
    /// line index so acting on an edge target jumps within the graph.
    fn load_cfg(&mut self, ctx: &Ctx) {
        let blocks = ctx.bn.cfg(&self.name);
        let rendered = crate::cfg::render(&blocks, self.cfg_graph);
        let text = rendered.lines.join("\n");
        self.lines = syntax::tokenize_plain(&text);
        self.cfg_index = rendered.index;
        self.stack_view.set_locals(&[]);
        self.locals = std::collections::HashMap::new();
        self.spans = build_spans(&self.lines, ctx, &self.locals);
        self.active = None;
        self.status = match rendered.note {
            Some(note) => format!(" {note}"),
            None => format!(
                " cfg · {} blocks · {} · v/i cycles view · Space {} graph",
                rendered.block_count,
                if rendered.graph { "graph" } else { "list" },
                if rendered.graph { "→ list" } else { "→ graph" },
            ),
        };
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
