//! Pure rendering of a function's control-flow graph for the viewer's CFG view,
//! from `bn`'s basic-block + edge data (`Bn::cfg`). Two layouts:
//!
//! - **graph** ([`graph`], default): a layered 2D box-and-arrow layout — the
//!   classic CFG picture. Blocks are ranked into layers (longest path from the
//!   entry), ordered within a layer to reduce edge crossings, and connected by
//!   orthogonally-routed arrows drawn on a character canvas. Long edges pass
//!   through dummy columns so they never cross a box; back-edges (loops) route up
//!   dedicated right-margin lanes. It lays out at its natural size and the viewer
//!   renders it as a **pannable canvas** (the viewport follows the selection), so
//!   width is unconstrained — it only defers to the list past [`MAX_GRAPH_BLOCKS`].
//!   Returns a [`GraphData`] (char grid + parallel colour grid + block rects).
//! - **list** ([`list`]): each block with its full instructions and labelled
//!   successor edges (`├─ true → block_1`), back-edges flagged `↑loop`. Scales to
//!   any size; returns a `block-start-address → line` index for in-place jumps.

use crate::bn::CfgBlock;
use crate::ctx::parse_hex;
use std::collections::HashMap;

/// Above this many blocks the 2D layout is skipped (too large to navigate as a
/// canvas); the view falls back to the list.
pub const MAX_GRAPH_BLOCKS: usize = 60;

// 2D-layout geometry.
const BOX_H: usize = 5; // 3 content rows + 2 borders
const V_GAP: usize = 3; // rows between rank bands (room for routing + labels)
const H_GAP: usize = 3; // columns between slots
const NODE_MIN_W: usize = 14; // min inner content width
const NODE_MAX_W: usize = 34; // max inner content width (keeps a box narrow)

/// A rendered CFG: the display lines, plus where each block's box/header landed
/// so the viewer can jump to a block by its start address.
pub struct Rendered {
    pub lines: Vec<String>,
    /// block start address -> index of the line to land the cursor on.
    pub index: HashMap<u64, usize>,
    pub block_count: usize,
}

/// A block with its parsed start address, in the order `bn` returned it (block 0
/// is the function's entry).
struct Block<'a> {
    start: u64,
    inner: &'a CfgBlock,
}

impl Block<'_> {
    fn start_str(&self) -> &str {
        &self.inner.start
    }
}

/// Parse the blocks, keeping only those with a valid `0x` start, and record the
/// entry (bn lists the entry block first). Returns the blocks sorted by address
/// (stable reading order) and the entry address.
fn prepare(blocks: &[CfgBlock]) -> (Vec<Block<'_>>, Option<u64>) {
    let mut parsed: Vec<Block> = blocks
        .iter()
        .filter_map(|b| parse_hex(&b.start).map(|start| Block { start, inner: b }))
        .collect();
    let entry = parsed.first().map(|b| b.start);
    parsed.sort_by_key(|b| b.start);
    (parsed, entry)
}

/// `block_<k>` labels keyed by start address, in the address-sorted display order.
fn labels_of(blocks: &[Block]) -> HashMap<u64, String> {
    blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.start, format!("block_{i}")))
        .collect()
}

/// Short word for an edge kind (`TrueBranch` -> `true`); empty for a plain
/// unconditional fallthrough.
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

/// The last meaningful instruction of a block (the terminator), skipping bn's
/// `{ … }` annotation lines. Empty when the block has none.
fn terminator(block: &Block) -> String {
    block
        .inner
        .insns
        .iter()
        .rev()
        .map(|i| i.t.trim())
        .find(|t| !t.is_empty() && !t.starts_with('{'))
        .unwrap_or("")
        .to_string()
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

// ---------------------------------------------------------------------------
// List layout (scalable fallback).
// ---------------------------------------------------------------------------

/// The `block_k  0xADDR` label for an edge target, or `0xADDR (external)` when
/// the target isn't a block of this function.
fn target_label(labels: &HashMap<u64, String>, to_str: &str) -> String {
    match parse_hex(to_str).and_then(|to| labels.get(&to)) {
        Some(label) => format!("{label}  {to_str}"),
        None => format!("{to_str}  (external)"),
    }
}

/// The successor-edge lines for a block in list mode.
fn edge_lines(block: &Block, labels: &HashMap<u64, String>, indent: &str) -> Vec<String> {
    let edges = &block.inner.edges;
    if edges.is_empty() {
        return vec![format!("{indent}└─ (returns)")];
    }
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
            let is_loop = parse_hex(&e.to).map(|to| to <= block.start).unwrap_or(false);
            let loop_note = if is_loop { "   ↑loop" } else { "" };
            let target = target_label(labels, &e.to);
            if word.is_empty() && word_w == 0 {
                format!("{indent}{connector}─▶ {target}{loop_note}")
            } else {
                format!("{indent}{connector} {word:word_w$} ─▶ {target}{loop_note}")
            }
        })
        .collect()
}

fn list_header(labels: &HashMap<u64, String>, block: &Block, entry: Option<u64>) -> String {
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

fn render_list(blocks: &[Block], labels: &HashMap<u64, String>, entry: Option<u64>) -> Rendered {
    let mut lines = Vec::new();
    let mut index = HashMap::new();
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        index.insert(block.start, lines.len());
        lines.push(list_header(labels, block, entry));
        for insn in &block.inner.insns {
            lines.push(format!("    {}  {}", insn.a, insn.t));
        }
        lines.extend(edge_lines(block, labels, "    "));
    }
    Rendered {
        lines,
        index,
        block_count: blocks.len(),
    }
}

// ---------------------------------------------------------------------------
// 2D layered graph layout.
// ---------------------------------------------------------------------------

// Stroke direction bits for the line-merging canvas.
const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

// Per-cell colour classes the viewer maps to styles.
pub const C_NONE: u8 = 0; // default
pub const C_TRUE: u8 = 1; // green — a true branch
pub const C_FALSE: u8 = 2; // red — a false branch
pub const C_OTHER: u8 = 3; // blue — unconditional / any other edge
pub const C_BORDER: u8 = 4; // box border (dim)
pub const C_TEXT: u8 = 5; // box content text
pub const C_ADDR: u8 = 6; // an address inside a box (cyan)

/// The colour class for an edge, from its `edge_word` (true→green, false→red,
/// anything else→blue).
fn edge_color(word: &str) -> u8 {
    match word {
        "true" => C_TRUE,
        "false" => C_FALSE,
        _ => C_OTHER,
    }
}

/// One block's box in the laid-out graph: where it sits on the canvas, plus its
/// label and full instruction listing (for selection/highlight, hjkl navigation,
/// and the always-on top-left block inspector).
pub struct GBlock {
    pub addr: u64,
    pub label: String,
    /// Full instructions: (address, disassembly text).
    pub insns: Vec<(String, String)>,
    pub top: usize,
    pub left: usize,
    pub w: usize,
    pub h: usize,
}

impl GBlock {
    pub fn cx(&self) -> usize {
        self.left + self.w / 2
    }
    pub fn cy(&self) -> usize {
        self.top + self.h / 2
    }
    pub fn contains(&self, row: usize, col: usize) -> bool {
        row >= self.top && row < self.top + self.h && col >= self.left && col < self.left + self.w
    }
}

/// A fully laid-out 2D control-flow graph: a character canvas with a parallel
/// colour grid, plus the block rectangles for navigation. `chars`/`color` are
/// row-major, `w * h`.
pub struct GraphData {
    pub w: usize,
    pub h: usize,
    pub chars: Vec<char>,
    pub color: Vec<u8>,
    pub blocks: Vec<GBlock>,
    /// Index into `blocks` of the function entry.
    pub entry: usize,
    pub block_count: usize,
}

impl GraphData {
    pub fn cell(&self, row: usize, col: usize) -> (char, u8) {
        if row < self.h && col < self.w {
            let i = row * self.w + col;
            (self.chars[i], self.color[i])
        } else {
            (' ', C_NONE)
        }
    }
}

/// The box-drawing glyph for a set of stroke directions.
fn glyph(mask: u8) -> char {
    match mask {
        m if m == UP | DOWN => '│',
        m if m == LEFT | RIGHT => '─',
        m if m == DOWN | RIGHT => '┌',
        m if m == DOWN | LEFT => '┐',
        m if m == UP | RIGHT => '└',
        m if m == UP | LEFT => '┘',
        m if m == UP | DOWN | RIGHT => '├',
        m if m == UP | DOWN | LEFT => '┤',
        m if m == DOWN | LEFT | RIGHT => '┬',
        m if m == UP | LEFT | RIGHT => '┴',
        m if m == UP | DOWN | LEFT | RIGHT => '┼',
        m if m & (UP | DOWN) != 0 => '│',
        m if m & (LEFT | RIGHT) != 0 => '─',
        _ => ' ',
    }
}

/// A character canvas with separate line-stroke masks (which merge into
/// junctions) and override glyphs (box content, arrowheads, labels).
struct Canvas {
    w: usize,
    h: usize,
    mask: Vec<u8>,
    ch: Vec<char>,
    color: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Canvas {
            w,
            h,
            mask: vec![0; w * h],
            ch: vec![' '; w * h],
            color: vec![C_NONE; w * h],
        }
    }

    fn idx(&self, r: usize, c: usize) -> Option<usize> {
        (r < self.h && c < self.w).then(|| r * self.w + c)
    }

    fn stroke(&mut self, r: usize, c: usize, bits: u8) {
        if let Some(i) = self.idx(r, c) {
            self.mask[i] |= bits;
        }
    }

    fn set(&mut self, r: usize, c: usize, glyph: char) {
        if let Some(i) = self.idx(r, c) {
            self.ch[i] = glyph;
        }
    }

    fn paint(&mut self, r: usize, c: usize, col: u8) {
        if let Some(i) = self.idx(r, c) {
            self.color[i] = col;
        }
    }

    fn text(&mut self, r: usize, c: usize, s: &str) {
        for (k, ch) in s.chars().enumerate() {
            self.set(r, c + k, ch);
        }
    }

    /// Colour every cell of a drawn polyline (call after [`path`]).
    fn paint_path(&mut self, cells: &[(usize, usize)], col: u8) {
        for &(r, c) in cells {
            self.paint(r, c, col);
        }
    }

    /// Draw a polyline through `cells`, recording the connecting stroke at each
    /// cell so corners/junctions merge correctly.
    fn path(&mut self, cells: &[(usize, usize)]) {
        for pair in cells.windows(2) {
            let (r0, c0) = pair[0];
            let (r1, c1) = pair[1];
            let (a, b) = if r1 > r0 || c1 > c0 {
                // moving down or right: first gets toward-bit, second the reverse
                match (r1 as i64 - r0 as i64, c1 as i64 - c0 as i64) {
                    (d, 0) if d > 0 => (DOWN, UP),
                    (_, d) if d > 0 => (RIGHT, LEFT),
                    _ => (0, 0),
                }
            } else {
                match (r1 as i64 - r0 as i64, c1 as i64 - c0 as i64) {
                    (d, 0) if d < 0 => (UP, DOWN),
                    (_, d) if d < 0 => (LEFT, RIGHT),
                    _ => (0, 0),
                }
            };
            self.stroke(r0, c0, a);
            self.stroke(r1, c1, b);
        }
    }

    /// Resolve to (chars, colors): each cell is its override glyph if set, else
    /// the box-drawing glyph for its accumulated strokes.
    fn resolve(self) -> (Vec<char>, Vec<u8>) {
        let mut chars = vec![' '; self.w * self.h];
        for i in 0..self.w * self.h {
            chars[i] = if self.ch[i] != ' ' {
                self.ch[i]
            } else {
                glyph(self.mask[i])
            };
        }
        (chars, self.color)
    }
}

/// A node in the layered layout: a real block or a routing dummy.
struct LNode {
    rank: usize,
    order: usize, // position within its rank
    cx: usize,    // center column (assigned after ordering)
    real: Option<usize>, // Some(block index) for a real box; None for a dummy
}

/// Detect back-edges (to an ancestor on the DFS stack) so ranking uses only the
/// forward DAG. Returns a bool per edge index.
fn back_edges(n: usize, edges: &[(usize, usize, String)], entry: usize) -> Vec<bool> {
    // adjacency of edge indices out of each node
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (ei, (u, _, _)) in edges.iter().enumerate() {
        out[*u].push(ei);
    }
    let mut state = vec![0u8; n]; // 0 unseen, 1 on-stack, 2 done
    let mut is_back = vec![false; edges.len()];
    // iterative DFS
    let mut stack: Vec<(usize, usize)> = vec![(entry, 0)];
    state[entry] = 1;
    while let Some(&(u, k)) = stack.last() {
        if k < out[u].len() {
            stack.last_mut().unwrap().1 += 1;
            let ei = out[u][k];
            let v = edges[ei].1;
            match state[v] {
                1 => is_back[ei] = true, // v on stack -> back edge
                0 => {
                    state[v] = 1;
                    stack.push((v, 0));
                }
                _ => {}
            }
        } else {
            state[u] = 2;
            stack.pop();
        }
    }
    // Any node unreached by the entry DFS: run DFS from it too, so its edges are
    // classified (defensive; a single-function CFG is usually fully reachable).
    for s in 0..n {
        if state[s] == 0 {
            state[s] = 1;
            stack.push((s, 0));
            while let Some(&(u, k)) = stack.last() {
                if k < out[u].len() {
                    stack.last_mut().unwrap().1 += 1;
                    let ei = out[u][k];
                    let v = edges[ei].1;
                    match state[v] {
                        1 => is_back[ei] = true,
                        0 => {
                            state[v] = 1;
                            stack.push((v, 0));
                        }
                        _ => {}
                    }
                } else {
                    state[u] = 2;
                    stack.pop();
                }
            }
        }
    }
    is_back
}

/// Longest-path rank (layer) per node over the forward edges.
fn ranks(n: usize, edges: &[(usize, usize, String)], is_back: &[bool]) -> Vec<usize> {
    let mut indeg = vec![0usize; n];
    let mut fwd: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (ei, (u, v, _)) in edges.iter().enumerate() {
        if !is_back[ei] {
            fwd[*u].push(*v);
            indeg[*v] += 1;
        }
    }
    let mut rank = vec![0usize; n];
    let mut queue: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let mut seen = 0;
    while let Some(u) = queue.pop() {
        seen += 1;
        for &v in &fwd[u] {
            if rank[u] + 1 > rank[v] {
                rank[v] = rank[u] + 1;
            }
            indeg[v] -= 1;
            if indeg[v] == 0 {
                queue.push(v);
            }
        }
    }
    // A cycle the back-edge detection missed would leave nodes unprocessed; they
    // keep rank 0, which is harmless (they just sit in the top band).
    let _ = seen;
    rank
}

/// One reduce-crossings sweep: reorder each layer (after the first) by the mean
/// order of each node's neighbours in the previous layer.
fn barycenter_down(layers: &mut [Vec<usize>], nodes: &[LNode], adj_up: &[Vec<usize>]) {
    let pos = |nodes: &[LNode], id: usize| nodes[id].order as f64;
    for r in 1..layers.len() {
        let prev_pos: HashMap<usize, f64> = layers[r - 1]
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i as f64))
            .collect();
        let mut keyed: Vec<(f64, usize)> = layers[r]
            .iter()
            .map(|&id| {
                let ups = &adj_up[id];
                let bc = if ups.is_empty() {
                    pos(nodes, id)
                } else {
                    ups.iter().filter_map(|u| prev_pos.get(u)).sum::<f64>()
                        / ups.len().max(1) as f64
                };
                (bc, id)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        layers[r] = keyed.into_iter().map(|(_, id)| id).collect();
    }
}

/// Build the compact 3-line content of a block's box.
fn box_content(labels: &HashMap<u64, String>, block: &Block, entry: Option<u64>) -> [String; 3] {
    let label = labels
        .get(&block.start)
        .cloned()
        .unwrap_or_else(|| "block".into());
    let n = block.inner.insns.len();
    let summary = if entry == Some(block.start) {
        format!("entry · {n} insns")
    } else {
        format!("{n} insns")
    };
    [
        format!("{label}  {}", block.start_str()),
        summary,
        terminator(block),
    ]
}

/// Lay out the 2D layered graph at its natural size. The caller renders it as a
/// pannable canvas, so width is unconstrained. `None` only on a degenerate
/// layout.
fn build_graph(
    blocks: &[Block],
    labels: &HashMap<u64, String>,
    entry: Option<u64>,
) -> Option<GraphData> {
    let n = blocks.len();
    let addr_idx: HashMap<u64, usize> = blocks.iter().enumerate().map(|(i, b)| (b.start, i)).collect();
    let entry_idx = entry.and_then(|a| addr_idx.get(&a).copied()).unwrap_or(0);

    // In-function edges (u, v, word). External targets are dropped.
    let mut edges: Vec<(usize, usize, String)> = Vec::new();
    for (u, b) in blocks.iter().enumerate() {
        for e in &b.inner.edges {
            if let Some(&v) = parse_hex(&e.to).and_then(|to| addr_idx.get(&to)) {
                edges.push((u, v, edge_word(&e.k).to_string()));
            }
        }
    }

    let is_back = back_edges(n, &edges, entry_idx);
    let rank = ranks(n, &edges, &is_back);
    let max_rank = *rank.iter().max().unwrap_or(&0);

    // Real layout nodes, one per block.
    let mut nodes: Vec<LNode> = (0..n)
        .map(|i| LNode {
            rank: rank[i],
            order: 0,
            cx: 0,
            real: Some(i),
        })
        .collect();

    // Forward edges spanning >1 rank get dummy nodes at the intermediate ranks,
    // so long edges occupy their own columns instead of crossing boxes. Each
    // forward edge records the chain of node ids it threads through.
    let mut fwd_chains: Vec<FwdChain> = Vec::new();
    let mut back_list: Vec<(usize, usize, u8)> = Vec::new(); // (from, to, colour)
    for (ei, (u, v, word)) in edges.iter().enumerate() {
        let color = edge_color(word);
        if is_back[ei] || rank[*v] <= rank[*u] {
            back_list.push((*u, *v, color));
            continue;
        }
        let mut chain = vec![*u];
        for r in (rank[*u] + 1)..rank[*v] {
            let id = nodes.len();
            nodes.push(LNode {
                rank: r,
                order: 0,
                cx: 0,
                real: None,
            });
            chain.push(id);
        }
        chain.push(*v);
        fwd_chains.push(FwdChain { chain, color });
    }

    // Layer membership.
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); max_rank + 1];
    for (id, node) in nodes.iter().enumerate() {
        layers[node.rank].push(id);
    }
    // Adjacency between consecutive chain nodes (for barycenter ordering).
    let mut adj_up: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for fc in &fwd_chains {
        for pair in fc.chain.windows(2) {
            adj_up[pair[1]].push(pair[0]);
        }
    }
    // Seed order, then a couple of down sweeps.
    for layer in &mut layers {
        for (i, &id) in layer.iter().enumerate() {
            nodes[id].order = i;
        }
    }
    for _ in 0..3 {
        barycenter_down(&mut layers, &nodes, &adj_up);
        for layer in &layers {
            for (i, &id) in layer.iter().enumerate() {
                nodes[id].order = i;
            }
        }
    }

    // Box widths (uniform), then x-positions (each layer centered on the canvas).
    let inner = blocks
        .iter()
        .map(|b| {
            box_content(labels, b, entry)
                .iter()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(NODE_MIN_W)
        .clamp(NODE_MIN_W, NODE_MAX_W);
    // Even width so the center column is exact and side lanes exit into the gap
    // one column past the right border (never onto it).
    let box_w = {
        let w = inner + 4; // "│ " + content + " │"
        if w % 2 == 0 {
            w
        } else {
            w + 1
        }
    };
    let slot_w = box_w; // dummies get a full box-width slot (a centered │)
    let core_w = layers
        .iter()
        .map(|layer| layer.len() * slot_w + layer.len().saturating_sub(1) * H_GAP)
        .max()
        .unwrap_or(box_w);
    let back_reserve = if back_list.is_empty() {
        0
    } else {
        2 + 2 * back_list.len()
    };
    let canvas_w = core_w + back_reserve;
    if canvas_w == 0 {
        return None;
    }

    for layer in &layers {
        let total = layer.len() * slot_w + layer.len().saturating_sub(1) * H_GAP;
        let start = (core_w - total) / 2;
        for (i, &id) in layer.iter().enumerate() {
            nodes[id].cx = start + i * (slot_w + H_GAP) + box_w / 2;
        }
    }

    let band = BOX_H + V_GAP;
    let canvas_h = (max_rank + 1) * BOX_H + max_rank * V_GAP;
    let mut canvas = Canvas::new(canvas_w, canvas_h + 1);
    // Block rectangles, placed by block index (address-sorted order).
    let mut gblocks: Vec<GBlock> = (0..n)
        .map(|_| GBlock {
            addr: 0,
            label: String::new(),
            insns: Vec::new(),
            top: 0,
            left: 0,
            w: box_w,
            h: BOX_H,
        })
        .collect();

    // Draw real boxes; draw dummy verticals.
    for node in &nodes {
        let top = node.rank * band;
        match node.real {
            Some(bi) => {
                let left = node.cx - box_w / 2;
                draw_box(&mut canvas, top, left, box_w, &box_content(labels, &blocks[bi], entry));
                gblocks[bi] = GBlock {
                    addr: blocks[bi].start,
                    label: labels
                        .get(&blocks[bi].start)
                        .cloned()
                        .unwrap_or_else(|| "block".into()),
                    insns: blocks[bi]
                        .inner
                        .insns
                        .iter()
                        .map(|i| (i.a.clone(), i.t.clone()))
                        .collect(),
                    top,
                    left,
                    w: box_w,
                    h: BOX_H,
                };
            }
            None => {
                for r in top..top + BOX_H {
                    canvas.stroke(r, node.cx, UP | DOWN);
                }
            }
        }
    }

    // Forward edges: orthogonal routes coloured by branch kind.
    route_forward(&mut canvas, &nodes, &fwd_chains, box_w, band);
    // Back-edges (loops): route up dedicated right-margin lanes.
    route_back(&mut canvas, &nodes, &back_list, core_w, box_w, band);

    let entry_idx = entry
        .and_then(|a| blocks.iter().position(|b| b.start == a))
        .unwrap_or(0);
    let (chars, color) = canvas.resolve();
    Some(GraphData {
        w: canvas_w,
        h: canvas_h + 1,
        chars,
        color,
        blocks: gblocks,
        entry: entry_idx,
        block_count: n,
    })
}

/// Draw a box with its 3 content lines. Borders use strokes (so edge exits merge
/// into `┬`/`┴`); content is written as text.
fn draw_box(canvas: &mut Canvas, top: usize, left: usize, w: usize, content: &[String; 3]) {
    let right = left + w - 1;
    let bottom = top + BOX_H - 1;
    // horizontal borders (interior columns only — corners set explicitly, so the
    // strokes don't OR into a `┼`)
    for c in (left + 1)..right {
        canvas.stroke(top, c, LEFT | RIGHT);
        canvas.stroke(bottom, c, LEFT | RIGHT);
    }
    // vertical borders (interior rows only)
    for r in (top + 1)..bottom {
        canvas.stroke(r, left, UP | DOWN);
        canvas.stroke(r, right, UP | DOWN);
    }
    // corners
    canvas.stroke(top, left, DOWN | RIGHT);
    canvas.stroke(top, right, DOWN | LEFT);
    canvas.stroke(bottom, left, UP | RIGHT);
    canvas.stroke(bottom, right, UP | LEFT);
    // colour the border cells dim
    for c in left..=right {
        canvas.paint(top, c, C_BORDER);
        canvas.paint(bottom, c, C_BORDER);
    }
    for r in top..=bottom {
        canvas.paint(r, left, C_BORDER);
        canvas.paint(r, right, C_BORDER);
    }
    // content — text plain, hex addresses cyan
    let inner = w.saturating_sub(4);
    for (i, line) in content.iter().enumerate() {
        let text = clip(line, inner);
        let row = top + 1 + i;
        canvas.text(row, left + 2, &text);
        paint_line_colors(canvas, row, left + 2, &text);
    }
}

/// Paint a box content line: hex `0x…` runs cyan, everything else default text.
fn paint_line_colors(canvas: &mut Canvas, row: usize, col: usize, text: &str) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '0' && i + 1 < chars.len() && chars[i + 1] == 'x' {
            let start = i;
            i += 2;
            while i < chars.len() && chars[i].is_ascii_hexdigit() {
                i += 1;
            }
            for c in start..i {
                canvas.paint(row, col + c, C_ADDR);
            }
        } else {
            canvas.paint(row, col + i, C_TEXT);
            i += 1;
        }
    }
}

/// Route every forward edge as an orthogonal connector between adjacent-rank
/// nodes along its chain, with a `▼` arrowhead at the real target and the edge
/// word as a small label near the source exit.
fn route_forward(
    canvas: &mut Canvas,
    nodes: &[LNode],
    chains: &[impl ChainLike],
    box_w: usize,
    band: usize,
) {
    // Per real node, gather out-segments (first hop of each chain) and
    // in-segments (last hop), to spread exit/entry points along the borders.
    // Exit/entry order follows the neighbour's column.
    let mut out_slots: HashMap<usize, Vec<(usize, usize)>> = HashMap::new(); // node -> [(neighbour_cx, chain_i)]
    let mut in_slots: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    for (ci, chain) in chains.iter().enumerate() {
        let c = chain.chain();
        let (u, u_next) = (c[0], c[1]);
        out_slots.entry(u).or_default().push((nodes[u_next].cx, ci));
        let (v, v_prev) = (c[c.len() - 1], c[c.len() - 2]);
        in_slots.entry(v).or_default().push((nodes[v_prev].cx, ci));
    }
    for slots in out_slots.values_mut() {
        slots.sort_by_key(|s| s.0);
    }
    for slots in in_slots.values_mut() {
        slots.sort_by_key(|s| s.0);
    }

    // Exit/entry column for (node, chain).
    let border_col = |node: usize, slots: &[(usize, usize)], ci: usize| -> usize {
        let left = nodes[node].cx - box_w / 2;
        let k = slots.iter().position(|&(_, c)| c == ci).unwrap_or(0);
        let count = slots.len();
        // spread across the interior of the border
        left + 1 + (k + 1) * (box_w.saturating_sub(2)) / (count + 1)
    };

    for (ci, chain) in chains.iter().enumerate() {
        let c = chain.chain();
        for hop in 0..c.len() - 1 {
            let upper = c[hop];
            let lower = c[hop + 1];
            let is_first = hop == 0;
            let is_last = hop + 1 == c.len() - 1;

            let up_bottom = nodes[upper].rank * band + BOX_H - 1;
            let ex = if is_first {
                let slots = &out_slots[&upper];
                border_col(upper, slots, ci)
            } else {
                nodes[upper].cx
            };
            let low_top = nodes[lower].rank * band;
            let tx = if is_last {
                let slots = &in_slots[&lower];
                border_col(lower, slots, ci)
            } else {
                nodes[lower].cx
            };

            // Route: down from exit into the gap, across a channel row, down into
            // the target top.
            let channel = up_bottom + 1 + V_GAP / 2;
            let mut cells = vec![(up_bottom, ex)];
            for r in (up_bottom + 1)..=channel {
                cells.push((r, ex));
            }
            let (lo, hi) = (ex.min(tx), ex.max(tx));
            if lo != hi {
                for cc in lo..=hi {
                    cells.push((channel, cc));
                }
            }
            for r in channel..=low_top {
                cells.push((r, tx));
            }
            canvas.path(&cells);
            canvas.paint_path(&cells, chain.color());

            // Arrowhead only into a real target (the final hop).
            if is_last && nodes[lower].real.is_some() && low_top > 0 {
                canvas.set(low_top - 1, tx, '▼');
                canvas.paint(low_top - 1, tx, chain.color());
            }
        }
    }
}

/// Route back-edges (loops) up dedicated right-margin lanes: out of the source's
/// right side, up the lane, and into the target's right side with a `▲`.
fn route_back(
    canvas: &mut Canvas,
    nodes: &[LNode],
    back_list: &[(usize, usize, u8)],
    core_w: usize,
    box_w: usize,
    band: usize,
) {
    for (i, &(u, v, color)) in back_list.iter().enumerate() {
        let lane = core_w + 1 + i * 2;
        let u_top = nodes[u].rank * band;
        let v_top = nodes[v].rank * band;
        let u_right = nodes[u].cx + box_w / 2;
        let v_right = nodes[v].cx + box_w / 2;
        let u_mid = u_top + BOX_H / 2;
        let v_mid = v_top + BOX_H / 2;
        let mut cells = vec![(u_mid, u_right)];
        for c in u_right..=lane {
            cells.push((u_mid, c));
        }
        let (lo, hi) = (v_mid.min(u_mid), v_mid.max(u_mid));
        for r in lo..=hi {
            cells.push((r, lane));
        }
        for c in (v_right..=lane).rev() {
            cells.push((v_mid, c));
        }
        canvas.path(&cells);
        canvas.paint_path(&cells, color);
        canvas.set(v_mid, v_right, '◀');
        canvas.paint(v_mid, v_right, color);
    }
}

/// A forward edge's threaded chain of layout-node ids (source, dummies…, target)
/// and its colour class.
struct FwdChain {
    chain: Vec<usize>,
    color: u8,
}

/// Lets `route_forward` take chains generically (keeps the routing signature
/// tidy and unit-testable).
trait ChainLike {
    fn chain(&self) -> &[usize];
    fn color(&self) -> u8;
}

impl ChainLike for FwdChain {
    fn chain(&self) -> &[usize] {
        &self.chain
    }
    fn color(&self) -> u8 {
        self.color
    }
}

// ---------------------------------------------------------------------------

/// Lay out a function's CFG as a 2D graph (rendered as a pannable canvas), when
/// it's within [`MAX_GRAPH_BLOCKS`]. `None` means the caller should fall back to
/// [`list`].
pub fn graph(blocks: &[CfgBlock]) -> Option<GraphData> {
    let (parsed, entry) = prepare(blocks);
    if parsed.is_empty() || parsed.len() > MAX_GRAPH_BLOCKS {
        return None;
    }
    let labels = labels_of(&parsed);
    build_graph(&parsed, &labels, entry)
}

/// Render a function's CFG as the scrollable block list (the fallback, and the
/// `Space`-toggled detail view).
pub fn list(blocks: &[CfgBlock]) -> Rendered {
    let (parsed, entry) = prepare(blocks);
    if parsed.is_empty() {
        return Rendered {
            lines: vec!["(no control-flow graph — function not found or has no blocks)".into()],
            index: HashMap::new(),
            block_count: 0,
        };
    }
    let labels = labels_of(&parsed);
    render_list(&parsed, &labels, entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bn::{CfgBlock, CfgEdge, CfgInsn};

    // FwdChain is private; expose it to the ChainLike impl via the module.
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

    /// Diamond: entry branches true→0x1020 / false→0x1010; both fall to a join
    /// at 0x1030, which loops back to entry then exits to 0x1040.
    fn diamond() -> Vec<CfgBlock> {
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

    fn as_text(g: &GraphData) -> String {
        g.chars.iter().collect()
    }

    #[test]
    fn graph_lays_out_2d_with_boxes_and_arrows() {
        let g = graph(&diamond()).expect("lays out");
        let text = as_text(&g);
        assert!(text.contains('┌') && text.contains('┐'), "has box corners");
        assert!(text.contains('▼'), "has forward arrowheads");
        assert!(text.contains("block_0"), "labels the entry");
        assert!(text.contains("0x1010") && text.contains("0x1020"), "both branches drawn");
        // Five blocks, entry recorded.
        assert_eq!(g.blocks.len(), 5);
        assert_eq!(g.blocks[g.entry].addr, 0x1000);
    }

    #[test]
    fn edges_are_coloured_by_branch_kind() {
        let g = graph(&diamond()).expect("lays out");
        assert!(g.color.contains(&C_TRUE), "true branch coloured green");
        assert!(g.color.contains(&C_FALSE), "false branch coloured red");
        // The unconditional joins (block_1/2 → block_3) are 'other' (blue).
        assert!(g.color.contains(&C_OTHER), "unconditional edge coloured blue");
    }

    #[test]
    fn graph_marks_the_loop_back_edge() {
        let g = graph(&diamond()).expect("lays out");
        // The join→entry back-edge routes up a lane with a ◀ head.
        assert!(as_text(&g).contains('◀'), "loop edge shown with a head");
    }

    #[test]
    fn graph_lays_out_regardless_of_width() {
        // Width is unconstrained now (the viewer pans a canvas), so a graph always
        // lays out up to the block cap — it never declines for being wide.
        let g = graph(&diamond()).expect("lays out at natural width");
        assert!(g.w > 0 && g.h > 0);
    }

    #[test]
    fn blocks_carry_addresses_and_rects() {
        let g = graph(&diamond()).expect("lays out");
        let addrs: Vec<u64> = g.blocks.iter().map(|b| b.addr).collect();
        for a in [0x1000, 0x1010, 0x1020, 0x1030, 0x1040] {
            assert!(addrs.contains(&a), "block {a:#x} present");
        }
        // Each rect's header row contains the block's address text.
        for b in &g.blocks {
            let row: String = (0..g.w).map(|c| g.cell(b.top + 1, c).0).collect();
            assert!(row.contains(&format!("{:#x}", b.addr)));
        }
    }

    #[test]
    fn list_mode_unchanged() {
        let r = list(&diamond());
        assert!(r.lines[0].starts_with("block_0  0x1000"));
        let joined = r.lines.join("\n");
        assert!(joined.contains("─▶ block_2  0x1020"));
        assert!(joined.contains("↑loop"));
        assert!(joined.contains("└─ (returns)"));
    }

    #[test]
    fn empty_blocks_yield_a_note_line() {
        let r = list(&[]);
        assert_eq!(r.block_count, 0);
        assert!(r.lines[0].contains("no control-flow graph"));
        assert!(graph(&[]).is_none());
    }

    /// Long forward edge (rank gap > 1) threads a dummy column, so it must not
    /// overwrite the intermediate box.
    #[test]
    fn long_edge_uses_a_dummy_column() {
        let blocks = vec![
            CfgBlock {
                start: "0x2000".into(),
                insns: vec![insn("0x2000", "b.eq 0x2020")],
                edges: vec![edge("0x2010", "FalseBranch"), edge("0x2020", "TrueBranch")],
            },
            CfgBlock {
                start: "0x2010".into(),
                insns: vec![insn("0x2010", "b 0x2020")],
                edges: vec![edge("0x2020", "UnconditionalBranch")],
            },
            CfgBlock {
                start: "0x2020".into(),
                insns: vec![insn("0x2020", "ret")],
                edges: vec![],
            },
        ];
        let g = graph(&blocks).expect("lays out");
        assert_eq!(g.blocks.len(), 3);
    }
}


