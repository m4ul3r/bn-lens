//! Code-viewer state and lifecycle.
//!
//! Interaction, rendering, and hotspot classification live in focused child
//! modules so this file stays the small shared model for the viewer.

mod actions;
mod hotspots;
mod input;
mod render;

use crate::ctx::Ctx;
use crate::syntax::{self, Line};
use hotspots::build_spans;

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

/// Which rendering of the current function/target we're showing. The three code
/// views cycle with `i`/`I`; Xrefs is entered with `x`.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Decomp,
    Mlil,
    Disasm,
    Xrefs,
}

impl View {
    fn is_code(self) -> bool {
        !matches!(self, View::Xrefs)
    }

    fn label(self) -> &'static str {
        match self {
            View::Decomp => "decompile",
            View::Mlil => "mlil",
            View::Disasm => "disasm",
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
    },
    Rename {
        old: String,
        buf: String,
        err: String,
    },
}

pub enum Exit {
    Stay,
    Back,
}

pub struct Viewer {
    name: String,
    view: View,
    lines: Vec<Line>,
    spans: Vec<Hotspot>,
    locals: std::collections::HashMap<String, String>, // name -> type (this fn)
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
}

impl Viewer {
    pub fn new(ctx: &Ctx, name: String, is_code: bool) -> Self {
        let mut viewer = Viewer {
            name,
            view: if is_code { View::Decomp } else { View::Xrefs },
            lines: Vec::new(),
            spans: Vec::new(),
            locals: std::collections::HashMap::new(),
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
        };
        viewer.load(ctx);
        viewer
    }

    pub(crate) fn is_composing_question(&self) -> bool {
        matches!(self.popup, Popup::Ask { .. })
    }

    fn load(&mut self, ctx: &Ctx) {
        let text = match self.view {
            View::Decomp => ctx.bn.decompile(&self.name),
            View::Mlil => ctx.bn.il(&self.name, "mlil"),
            View::Disasm => ctx.bn.disasm(&self.name),
            View::Xrefs => ctx.bn.xrefs(&self.name),
        };
        self.lines = if matches!(self.view, View::Decomp) {
            syntax::tokenize_c(&text)
        } else {
            syntax::tokenize_plain(&text)
        };
        // Locals are meaningful for the named views (decomp/mlil), not disasm/xrefs.
        self.locals = if matches!(self.view, View::Decomp | View::Mlil) {
            ctx.bn.local_list(&self.name)
        } else {
            std::collections::HashMap::new()
        };
        let current = self.view.is_code().then_some(self.name.as_str());
        self.spans = build_spans(&self.lines, ctx, &self.locals, current);
        self.active = None;
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
