//! Pure rendering of a function's control-flow graph into text lines for the
//! viewer's CFG view. Two layouts, both driven off `bn`'s basic-block + edge
//! data (`Bn::cfg`):
//!
//! - **list** (default): each basic block as an entry — its address, its
//!   instructions, and its labelled successor edges (`├─ true → block_1`), with
//!   back-edges marked `↑loop`. Reads top-to-bottom in address order and stays
//!   legible at any block count.
//! - **graph** (size-gated toggle): the same blocks wrapped in boxes with
//!   arrow connectors — the ascii-graph look — but only attempted for small
//!   functions, since a fixed character grid can't route a big CFG readably.
//!
//! Both return a `block-start-address → header-line` index so the viewer can
//! jump between blocks in-place when you act on an edge target.

use crate::bn::CfgBlock;
use crate::ctx::parse_hex;
use std::collections::HashMap;

/// Above this many blocks the box-graph layout is suppressed (it can't fit a
/// fixed-width terminal readably); the view falls back to the list with a note.
pub const MAX_GRAPH_BLOCKS: usize = 24;

/// Widest instruction text a graph box will show before truncating (keeps a box
/// from stretching past a typical pane).
const BOX_INNER_MAX: usize = 72;
const BOX_INNER_MIN: usize = 30;

/// A rendered CFG: the display lines, plus where each block's header landed so
/// the viewer can jump to a block by its start address.
pub struct Rendered {
    pub lines: Vec<String>,
    /// block start address -> index of its header line in `lines`.
    pub index: HashMap<u64, usize>,
    pub block_count: usize,
    /// Whether the box-graph layout was actually used (false = list fallback).
    pub graph: bool,
    /// A one-line status note (e.g. why graph layout was suppressed), if any.
    pub note: Option<String>,
}

/// A block with its parsed start address, in the order `bn` returned it (block 0
/// is the function's entry).
struct Block<'a> {
    start: u64,
    inner: &'a CfgBlock,
}

/// Parse the blocks, keeping only those with a valid `0x` start, and record the
/// entry (bn lists the entry block first). Returns the blocks sorted by address
/// (stable top-to-bottom reading) and the entry address.
fn prepare(blocks: &[CfgBlock]) -> (Vec<Block<'_>>, Option<u64>) {
    let mut parsed: Vec<Block> = blocks
        .iter()
        .filter_map(|b| parse_hex(&b.start).map(|start| Block { start, inner: b }))
        .collect();
    let entry = parsed.first().map(|b| b.start);
    parsed.sort_by_key(|b| b.start);
    (parsed, entry)
}

/// `block_<k>` labels keyed by start address, assigned in the address-sorted
/// order the blocks display in.
fn labels_of(blocks: &[Block]) -> HashMap<u64, String> {
    blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.start, format!("block_{i}")))
        .collect()
}

/// Short word for an edge kind (`TrueBranch` -> `true`); empty for a plain
/// unconditional fallthrough (drawn as a bare arrow).
fn edge_word(kind: &str) -> &str {
    match kind {
        "TrueBranch" => "true",
        "FalseBranch" => "false",
        "UnconditionalBranch" => "",
        "FunctionReturn" => "ret",
        "IndirectBranch" => "jump*",
        "ExceptionBranch" => "except",
        other => other,
    }
}

/// The `block_k  0xADDR` label for an edge target, or `0xADDR  (external)` when
/// the target isn't a block of this function.
fn target_label(labels: &HashMap<u64, String>, to_str: &str) -> String {
    match parse_hex(to_str).and_then(|to| labels.get(&to)) {
        Some(label) => format!("{label}  {to_str}"),
        None => format!("{to_str}  (external)"),
    }
}

/// The successor-edge lines for a block, indented by `indent`. `├─`/`└─`
/// connectors; a target below its source is flagged `↑loop`.
fn edge_lines(block: &Block, labels: &HashMap<u64, String>, indent: &str) -> Vec<String> {
    let edges = &block.inner.edges;
    if edges.is_empty() {
        return vec![format!("{indent}└─ (returns)")];
    }
    // Width of the widest edge word, so the `─▶` arrows line up.
    let word_w = edges
        .iter()
        .map(|e| edge_word(&e.k).chars().count())
        .max()
        .unwrap_or(0);
    edges
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let connector = if i + 1 == edges.len() { "└─" } else { "├─" };
            let word = edge_word(&e.k);
            let loop_flag = parse_hex(&e.to)
                .map(|to| to <= block.start)
                .unwrap_or(false);
            let loop_note = if loop_flag { "   ↑loop" } else { "" };
            let target = target_label(labels, &e.to);
            if word.is_empty() && word_w == 0 {
                format!("{indent}{connector}─▶ {target}{loop_note}")
            } else {
                format!("{indent}{connector} {word:word_w$} ─▶ {target}{loop_note}")
            }
        })
        .collect()
}

/// Header text for a block: `block_k  0xADDR  [entry]  (N insns)`.
fn header(labels: &HashMap<u64, String>, block: &Block, entry: Option<u64>) -> String {
    let label = labels
        .get(&block.start)
        .cloned()
        .unwrap_or_else(|| "block".into());
    let tag = if entry == Some(block.start) {
        "  entry"
    } else {
        ""
    };
    format!(
        "{label}  {}{tag}  ({} insns)",
        block.start_str(),
        block.inner.insns.len()
    )
}

impl Block<'_> {
    fn start_str(&self) -> &str {
        &self.inner.start
    }
}

/// Render as the flat, always-legible block list.
fn render_list(blocks: &[Block], labels: &HashMap<u64, String>, entry: Option<u64>) -> Rendered {
    let mut lines = Vec::new();
    let mut index = HashMap::new();
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        index.insert(block.start, lines.len());
        lines.push(header(labels, block, entry));
        for insn in &block.inner.insns {
            lines.push(format!("    {}  {}", insn.a, insn.t));
        }
        lines.extend(edge_lines(block, labels, "    "));
    }
    Rendered {
        lines,
        index,
        block_count: blocks.len(),
        graph: false,
        note: None,
    }
}

/// Truncate `text` to `max` chars with an ellipsis.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Render as boxed blocks with arrow connectors (the ascii-graph look). Only
/// called when the block count is within [`MAX_GRAPH_BLOCKS`].
fn render_graph(blocks: &[Block], labels: &HashMap<u64, String>, entry: Option<u64>) -> Rendered {
    // Box inner width fits the widest header/instruction, bounded.
    let inner = blocks
        .iter()
        .flat_map(|b| {
            std::iter::once(header(labels, b, entry).chars().count() + 2).chain(
                b.inner
                    .insns
                    .iter()
                    .map(|i| format!("{}  {}", i.a, i.t).chars().count()),
            )
        })
        .max()
        .unwrap_or(BOX_INNER_MIN)
        .clamp(BOX_INNER_MIN, BOX_INNER_MAX);

    let mut lines = Vec::new();
    let mut index = HashMap::new();
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        index.insert(block.start, lines.len());
        // Top border carries the header.
        let head = header(labels, block, entry);
        let head = clip(&head, inner);
        let dashes = inner + 1 - head.chars().count();
        lines.push(format!("┌ {head} {}┐", "─".repeat(dashes.saturating_sub(1))));
        for insn in &block.inner.insns {
            let body = clip(&format!("{}  {}", insn.a, insn.t), inner);
            lines.push(format!("│ {body:inner$} │"));
        }
        lines.push(format!("└{}┘", "─".repeat(inner + 2)));
        lines.extend(edge_lines(block, labels, "   "));
    }
    Rendered {
        lines,
        index,
        block_count: blocks.len(),
        graph: true,
        note: None,
    }
}

/// Render a function's CFG. `want_graph` requests the boxed layout, honoured
/// only when the block count is small enough; otherwise the list is returned
/// with a note explaining the fallback.
pub fn render(blocks: &[CfgBlock], want_graph: bool) -> Rendered {
    let (parsed, entry) = prepare(blocks);
    if parsed.is_empty() {
        return Rendered {
            lines: vec!["(no control-flow graph — function not found or has no blocks)".into()],
            index: HashMap::new(),
            block_count: 0,
            graph: false,
            note: None,
        };
    }
    let labels = labels_of(&parsed);
    if want_graph {
        if parsed.len() <= MAX_GRAPH_BLOCKS {
            return render_graph(&parsed, &labels, entry);
        }
        let mut rendered = render_list(&parsed, &labels, entry);
        rendered.note = Some(format!(
            "graph layout suppressed — {} blocks (too wide to draw); showing list",
            parsed.len()
        ));
        return rendered;
    }
    render_list(&parsed, &labels, entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bn::{CfgBlock, CfgEdge, CfgInsn};

    fn insn(a: &str, t: &str) -> CfgInsn {
        CfgInsn {
            a: a.into(),
            t: t.into(),
        }
    }
    fn edge(to: &str, k: &str) -> CfgEdge {
        CfgEdge {
            to: to.into(),
            k: k.into(),
        }
    }

    /// A tiny diamond: entry branches true/false to two blocks that both
    /// fall through to a join block; the join loops back to entry.
    fn sample() -> Vec<CfgBlock> {
        vec![
            CfgBlock {
                start: "0x1000".into(),
                insns: vec![insn("0x1000", "cbz x0, 0x1020")],
                edges: vec![edge("0x1020", "TrueBranch"), edge("0x1010", "FalseBranch")],
            },
            CfgBlock {
                start: "0x1010".into(),
                insns: vec![insn("0x1010", "mov x1, #1")],
                edges: vec![edge("0x1030", "UnconditionalBranch")],
            },
            CfgBlock {
                start: "0x1020".into(),
                insns: vec![insn("0x1020", "mov x1, #2")],
                edges: vec![edge("0x1030", "UnconditionalBranch")],
            },
            CfgBlock {
                start: "0x1030".into(),
                insns: vec![insn("0x1030", "cbnz x2, 0x1000")],
                edges: vec![edge("0x1000", "TrueBranch"), edge("0x1040", "FalseBranch")],
            },
            CfgBlock {
                start: "0x1040".into(),
                insns: vec![insn("0x1040", "ret")],
                edges: vec![],
            },
        ]
    }

    #[test]
    fn list_orders_by_address_and_labels_entry() {
        let r = render(&sample(), false);
        assert!(!r.graph);
        assert_eq!(r.block_count, 5);
        // Entry (bn's first block, 0x1000) is labelled block_0 and tagged entry.
        assert!(r.lines[0].starts_with("block_0  0x1000"));
        assert!(r.lines[0].contains("entry"));
        // Address-sorted: block_1 is 0x1010, not the true-branch 0x1020.
        let joined = r.lines.join("\n");
        assert!(joined.contains("block_1  0x1010"));
        assert!(joined.contains("block_2  0x1020"));
    }

    #[test]
    fn edges_are_labelled_and_backedges_flagged() {
        let r = render(&sample(), false);
        let joined = r.lines.join("\n");
        // Entry's conditional edges resolve to block labels.
        assert!(joined.contains("true") && joined.contains("─▶ block_2  0x1020"));
        assert!(joined.contains("false") && joined.contains("─▶ block_1  0x1010"));
        // The join block's TrueBranch back to entry (0x1000 <= 0x1030) is a loop.
        assert!(joined.contains("─▶ block_0  0x1000   ↑loop"));
        // The return block shows no successors.
        assert!(joined.contains("└─ (returns)"));
    }

    #[test]
    fn index_points_at_each_block_header() {
        let r = render(&sample(), false);
        for (addr, &line) in &r.index {
            let text = &r.lines[line];
            let addr_str = format!("0x{addr:x}");
            assert!(
                text.contains(&addr_str) && text.contains("block_"),
                "index for {addr_str} points at non-header line {text:?}"
            );
        }
        assert_eq!(r.index.len(), 5);
    }

    #[test]
    fn graph_mode_boxes_small_functions() {
        let r = render(&sample(), true);
        assert!(r.graph);
        assert!(r.note.is_none());
        assert!(r.lines.iter().any(|l| l.starts_with("┌ block_0")));
        assert!(r.lines.iter().any(|l| l.starts_with("│ ")));
        assert!(r.lines.iter().any(|l| l.starts_with("└─")));
        // The header line is still the indexed jump target.
        let head = r.index[&0x1000];
        assert!(r.lines[head].contains("0x1000"));
    }

    #[test]
    fn graph_suppressed_for_large_functions() {
        let mut blocks = Vec::new();
        for i in 0..(MAX_GRAPH_BLOCKS + 1) {
            let a = 0x2000 + i * 0x10;
            blocks.push(CfgBlock {
                start: format!("0x{a:x}"),
                insns: vec![insn(&format!("0x{a:x}"), "nop")],
                edges: vec![edge(&format!("0x{:x}", a + 0x10), "UnconditionalBranch")],
            });
        }
        let r = render(&blocks, true);
        assert!(!r.graph, "too many blocks -> list fallback");
        assert!(r.note.as_deref().unwrap().contains("suppressed"));
    }

    #[test]
    fn empty_blocks_yield_a_note_line() {
        let r = render(&[], false);
        assert_eq!(r.block_count, 0);
        assert!(r.lines[0].contains("no control-flow graph"));
        assert!(r.index.is_empty());
    }
}
