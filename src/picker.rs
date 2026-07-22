//! The function picker: an address-ordered list of *all* functions, with a
//! "recently viewed" subsection on top — functions you opened here (`▸ you`)
//! and functions/addresses the agent named in its pane (`◆ agent`), refreshed
//! live. Non-function addresses are annotated with their section + nearest
//! symbol (so a `.bss` global reads as `.bss → srv_state`, not `(addr)`).

use crate::ctx::Ctx;
use crate::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::collections::{HashMap, HashSet};

pub enum Action {
    OpenDecompile(String),
    OpenXrefs(String),
    Switch,
    /// Return to the Symbols list (Esc from a non-Symbols list, filter empty).
    Home,
    Quit,
    None,
}

const YOU: u8 = 1;
const AGENT: u8 = 2;
const RECENT_CAP: usize = 12;

#[derive(Clone)]
struct Item {
    addr: String,
    name: String,   // "" => a data address (not a function)
    target: String, // decompile/xref target: fn name, or the address
    import: bool,
    /// The name is BN's `sub_<addr>` placeholder — i.e. it carries no
    /// information the address column doesn't already show.
    auto: bool,
    annot: String, // section → nearest-symbol for data addresses; "" for functions
    src: u8,       // YOU|AGENT bits (0 in the "all" group)
}

/// Row-class filter (`f`): which functions the "all" group admits.
#[derive(Clone, Copy, PartialEq)]
enum Class {
    All,
    Named,
    Unnamed,
    Import,
}

impl Class {
    fn next(self) -> Class {
        match self {
            Class::All => Class::Named,
            Class::Named => Class::Unnamed,
            Class::Unnamed => Class::Import,
            Class::Import => Class::All,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Class::All => "all",
            Class::Named => "named",
            Class::Unnamed => "unnamed",
            Class::Import => "imports",
        }
    }
    fn admits(self, it: &Item) -> bool {
        match self {
            Class::All => true,
            Class::Named => !it.import && !it.auto,
            Class::Unnamed => !it.import && it.auto,
            Class::Import => it.import,
        }
    }
}

/// Sort order for the "all" group (`o`).
#[derive(Clone, Copy, PartialEq)]
enum Order {
    Addr,
    Size,
    Blocks,
}

impl Order {
    fn next(self) -> Order {
        match self {
            Order::Addr => Order::Size,
            Order::Size => Order::Blocks,
            Order::Blocks => Order::Addr,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Order::Addr => "by address",
            Order::Size => "by size",
            Order::Blocks => "by blocks",
        }
    }
}

/// One display line: a plain label, a foldable section rule, or an item in the
/// recent / all group.
enum Row {
    /// A non-selectable label (the recent group's legend).
    Header(String),
    /// A section rule, carrying its index into `ranges`. Selectable: Enter or
    /// Space folds it, so `.plt` can be reduced to a single line.
    Section(usize, String),
    Recent(usize),
    All(usize),
}

enum Mode {
    Normal,
    Search,
}

pub struct Picker {
    all: Vec<Item>,     // every function, by address (stable)
    recent: Vec<Item>,  // rebuilt from opens + agent scan
    opens: Vec<String>, // fn names you opened here (MRU, newest first)
    agent: Vec<String>, // tokens from the agent's pane (most-recent first)
    awidth: usize,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sel: usize, // index into the current rows()
    top: usize,
    pending_g: bool,
    // classification + annotation tables (static per target)
    fn_names: HashSet<String>,
    fn_name_addr: HashMap<String, String>, // raw/display alias -> addr
    fn_alias_target: HashMap<String, String>, // raw/display alias -> stable raw name
    fn_addr_name: HashMap<String, String>, // addr -> stable raw name
    fn_display: HashMap<String, String>,   // stable raw name -> display name
    imports: HashSet<String>,
    ranges: Vec<(u64, u64, String)>, // section ranges
    syms: Vec<(u64, String)>,        // symbol addr -> name, sorted ascending
    sec_lines: Vec<String>,          // raw `bn sections` for the `s` popup
    sec_off: Option<usize>,
    // addr -> (byte size, basic-block count) for the picker's right-margin
    // columns; keyed by function address so recent + all rows share it.
    meta_by_addr: HashMap<String, (u64, u32)>,
    /// Index into `ranges` of the section each `all` entry lives in (parallel to
    /// `all`; `usize::MAX` when it falls outside every section). Precomputed so
    /// the per-section rules in `rows()` stay O(n).
    all_sec: Vec<usize>,
    /// Basic-block count at the 90th percentile — the "hairy function" accent
    /// threshold for the size bar. `u32::MAX` on targets too small to rank.
    bb_hot: u32,
    /// Width of the name field for this target (see [`name_cap`]).
    namew_cap: usize,
    class: Class,
    order: Order,
    /// Folded sections, by index into `ranges` (`usize::MAX` = the no-section
    /// bucket). Survives a ctx refresh — a fold is a reading position, and
    /// re-expanding `.plt` after every rename would be its own annoyance.
    collapsed: HashSet<usize>,
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim().strip_prefix("0x")?, 16).ok()
}

/// Human-readable byte size for the picker's right margin: `20`, `1.2K`, `3.4M`.
fn fmt_size(n: u64) -> String {
    if n < 1024 {
        format!("{n}")
    } else if n < 1024 * 1024 {
        format!("{:.1}K", n as f64 / 1024.0)
    } else {
        format!("{:.1}M", n as f64 / (1024.0 * 1024.0))
    }
}

/// The right-margin stats column (`  1.2K   8 bb`) for a function row, or empty
/// when there's nothing meaningful to show. Import stubs are uniformly tiny, so
/// their size/block numbers are noise — suppressed by the caller via `import`.
fn stats_col(size: u64, bbc: u32) -> String {
    format!("{:>7}  {:>4} bb", fmt_size(size), bbc)
}

/// Column width of `stats_col` output (fixed: 7 + 2 + 4 + 3). The block field is
/// four wide because real functions reach four digits — a 2 MB stripped daemon
/// has entries past 1000 blocks, and a narrower field shifts every such row out
/// of alignment with the rest of the column.
const STATS_W: usize = 16;
/// Don't render stats unless the name field keeps at least this many columns.
const STATS_MIN_NAME: usize = 12;
/// Width of the log-scaled size bar.
const BAR_W: usize = 10;
/// The name field never grows past this, however wide the pane. Padding a name
/// out to 120 columns strands the metrics on the far right with nothing tying
/// them to the row; a capped field keeps the whole record inside one saccade.
const NAME_MAX: usize = 48;
/// Stand-in drawn for a `sub_<addr>` name — the address column already said it.
const NO_NAME: &str = "·";

/// True when `name` is a BN placeholder rather than recovered information:
/// `sub_<addr>` (the address column already says it) or `j_sub_<addr>` (a thunk
/// into a function that is itself unnamed). A thunk to a *named* function —
/// `j_memcpy` — is real information and stays named.
fn is_auto_name(name: &str, addr: &str) -> bool {
    if let Some(hex) = name.strip_prefix("j_sub_") {
        return u64::from_str_radix(hex, 16).is_ok();
    }
    let Some(hex) = name.strip_prefix("sub_") else {
        return false;
    };
    match (u64::from_str_radix(hex, 16), parse_hex(addr)) {
        (Ok(v), Some(a)) => v == a,
        _ => false,
    }
}

/// What to draw in place of a placeholder name. A `sub_<addr>` says nothing the
/// row doesn't already, so it collapses to a marker; a thunk still points
/// somewhere, and where it points is the only thing about it worth reading.
fn auto_label(name: &str) -> String {
    match name.strip_prefix("j_sub_") {
        Some(hex) => format!("→ 0x{hex}"),
        None => NO_NAME.to_string(),
    }
}

/// A log-scaled magnitude bar for a function body, in eighth-cell blocks.
/// A thunk reads as a sliver and 16 KiB fills the field, so scrolling the list
/// renders the *shape* of the binary — stub plateaus, then real code. The floor
/// and ceiling are set to the span real function bodies actually occupy, so the
/// bar spends its ten cells discriminating between them rather than reserving
/// headroom nothing reaches.
fn size_bar(size: u64, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    let l = (size.max(1) as f64).log2();
    let frac = ((l - 3.0) / 11.0).clamp(0.0, 1.0);
    let eighths = ((frac * (w * 8) as f64).round() as usize).clamp(1, w * 8);
    let (full, rem) = (eighths / 8, eighths % 8);
    let mut s = "█".repeat(full.min(w));
    if rem > 0 && full < w {
        s.push(['▏', '▎', '▍', '▌', '▋', '▊', '▉'][rem - 1]);
    }
    s
}

/// Floor for the name field, so it still reads as a column on a target with no
/// recovered names at all.
const NAME_MIN: usize = 10;

/// Split the columns right of the address gutter into (name, bar, stats) widths.
/// Everything is packed left and capped at `cap`, so the surplus on a wide pane
/// becomes trailing whitespace instead of a gulf inside the row.
fn columns(rest: usize, cap: usize) -> (usize, usize, usize) {
    if rest >= STATS_MIN_NAME + 2 + BAR_W + STATS_W {
        let name = (rest - 2 - BAR_W - STATS_W).min(cap);
        (name, BAR_W, STATS_W)
    } else if rest >= STATS_MIN_NAME + STATS_W {
        ((rest - STATS_W).min(cap), 0, STATS_W)
    } else {
        (rest, 0, 0)
    }
}

/// How wide the name field needs to be for *this* target: the longest recovered
/// name, floored and capped. On a stripped binary almost every row renders the
/// no-name marker, and a 48-column field of markers is 48 columns of nothing —
/// sizing to the names that exist pulls the metrics back next to the addresses.
fn name_cap(all: &[Item]) -> usize {
    all.iter()
        .filter(|it| !it.auto && !it.import && !it.name.is_empty())
        .map(|it| it.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(NAME_MIN, NAME_MAX)
}

/// Truncate `s` to at most `maxw` chars, ending in `…` when clipped.
fn truncate(s: &str, maxw: usize) -> String {
    if s.chars().count() <= maxw {
        return s.to_string();
    }
    if maxw == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(maxw - 1).collect();
    out.push('…');
    out
}

impl Picker {
    pub fn new(ctx: &Ctx) -> Self {
        let awidth = ctx
            .funcs
            .iter()
            .map(|f| f.addr.len())
            .max()
            .unwrap_or(10)
            .max(10);

        let mut all: Vec<Item> = ctx
            .funcs
            .iter()
            .map(|f| Item {
                addr: f.addr.clone(),
                name: f.display_name.clone(),
                target: f.name.clone(),
                import: ctx.import_names.contains(&f.name)
                    || ctx.import_names.contains(&f.display_name),
                auto: is_auto_name(&f.display_name, &f.addr),
                annot: String::new(),
                src: 0,
            })
            .collect();
        all.sort_by_key(|it| parse_hex(&it.addr).unwrap_or(0));

        let fn_names = ctx.func_names.clone();
        let mut fn_name_addr = HashMap::new();
        let mut fn_alias_target = HashMap::new();
        let mut fn_display = HashMap::new();
        for f in &ctx.funcs {
            fn_name_addr.insert(f.name.clone(), f.addr.clone());
            fn_name_addr.insert(f.display_name.clone(), f.addr.clone());
            fn_alias_target.insert(f.name.clone(), f.name.clone());
            fn_alias_target.insert(f.display_name.clone(), f.name.clone());
            fn_display.insert(f.name.clone(), f.display_name.clone());
        }
        let fn_addr_name: HashMap<String, String> = ctx
            .funcs
            .iter()
            .map(|f| (f.addr.clone(), f.name.clone()))
            .collect();
        let meta_by_addr: HashMap<String, (u64, u32)> = ctx
            .funcs
            .iter()
            .map(|f| (f.addr.clone(), (f.size, f.basic_block_count)))
            .collect();

        let mut syms: Vec<(u64, String)> = ctx
            .addr_by_name
            .iter()
            .filter_map(|(n, a)| parse_hex(a).map(|v| (v, n.clone())))
            .collect();
        syms.sort_by_key(|(a, _)| *a);

        let sec_lines = ctx.sections_text.clone();
        let mut ranges: Vec<(u64, u64, String)> = ctx
            .section_ranges
            .iter()
            .map(|(s, e, n, _)| (*s, *e, n.clone()))
            .collect();
        // Address order is section order: sorting here lets the list's section
        // rules fall out of a single pass over the address-sorted functions.
        ranges.sort_by_key(|(s, _, _)| *s);

        let all_sec: Vec<usize> = all
            .iter()
            .map(|it| {
                let a = parse_hex(&it.addr).unwrap_or(0);
                ranges
                    .iter()
                    .position(|(s, e, _)| a >= *s && a < *e)
                    .unwrap_or(usize::MAX)
            })
            .collect();

        // Accent threshold for the size bar: the 98th percentile of block count
        // among functions that actually branch. Ranking over *all* functions
        // puts the bar low enough that a third of the list lights up, which is
        // no longer an accent — stubs and thunks (bbc < 2) are excluded so the
        // percentile describes real code. Below ~50 such functions a percentile
        // says nothing, so nothing accents.
        let mut bbs: Vec<u32> = ctx
            .funcs
            .iter()
            .map(|f| f.basic_block_count)
            .filter(|&b| b >= 2)
            .collect();
        bbs.sort_unstable();
        let bb_hot = if bbs.len() < 50 {
            u32::MAX
        } else {
            bbs[bbs.len() * 98 / 100].max(8)
        };

        let namew_cap = name_cap(&all);

        Picker {
            all,
            recent: Vec::new(),
            opens: Vec::new(),
            agent: Vec::new(),
            awidth,
            filter: String::new(),
            prev_filter: String::new(),
            mode: Mode::Normal,
            sel: 0,
            top: 0,
            pending_g: false,
            fn_names,
            fn_name_addr,
            fn_alias_target,
            fn_addr_name,
            fn_display,
            imports: ctx.import_names.clone(),
            ranges,
            syms,
            sec_lines,
            sec_off: None,
            meta_by_addr,
            all_sec,
            bb_hot,
            namew_cap,
            class: Class::All,
            order: Order::Addr,
            collapsed: HashSet::new(),
        }
    }

    /// Re-sync the static tables (functions, symbols, sections) from a rebuilt
    /// ctx after a mutation/refresh, preserving your opens, the agent scan, the
    /// filter, and the cursor. A fresh `Picker` would drop that live history.
    pub fn refresh(&mut self, ctx: &Ctx) {
        let fresh = Picker::new(ctx);
        self.all = fresh.all;
        self.awidth = fresh.awidth;
        self.fn_names = fresh.fn_names;
        self.fn_name_addr = fresh.fn_name_addr;
        self.fn_alias_target = fresh.fn_alias_target;
        self.fn_addr_name = fresh.fn_addr_name;
        self.fn_display = fresh.fn_display;
        self.imports = fresh.imports;
        self.ranges = fresh.ranges;
        self.syms = fresh.syms;
        self.sec_lines = fresh.sec_lines;
        self.meta_by_addr = fresh.meta_by_addr;
        self.all_sec = fresh.all_sec;
        self.bb_hot = fresh.bb_hot;
        self.namew_cap = fresh.namew_cap;
        self.rebuild_recent();
    }

    /// True while the search filter is capturing raw text (so App must not steal
    /// `m`/`?`/`^R` as global shortcuts — they belong in the query).
    pub fn is_searching(&self) -> bool {
        matches!(self.mode, Mode::Search)
    }

    /// True while a self-managed overlay (the sections popup) owns input.
    pub fn popup_open(&self) -> bool {
        self.sec_off.is_some()
    }

    /// Record a function you just opened in the lens (MRU, newest first).
    pub fn record_open(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        self.opens.retain(|n| n != name);
        self.opens.insert(0, name.to_string());
        self.opens.truncate(RECENT_CAP);
        self.rebuild_recent();
    }

    /// Update the agent-referenced tokens (from a fresh scan of its pane).
    pub fn update_agent(&mut self, tokens: Vec<String>) {
        if tokens == self.agent {
            return;
        }
        self.agent = tokens;
        self.rebuild_recent();
    }

    /// Section + nearest-symbol annotation for a data address.
    fn annotate(&self, addr: u64) -> String {
        let sec = self.ranges.iter().find(|(s, e, _)| addr >= *s && addr < *e);
        let sec_start = sec.map(|(s, _, _)| *s).unwrap_or(0);
        // nearest symbol at or below the address, but within the same section
        let sym = self
            .syms
            .iter()
            .rev()
            .find(|(a, _)| *a <= addr && *a >= sec_start);
        match (sec, sym) {
            (Some((_, _, name)), Some((sa, sn))) => {
                let off = addr - sa;
                if off == 0 {
                    format!("{name} → {sn}")
                } else {
                    format!("{name} → {sn} +{off:#x}")
                }
            }
            (Some((_, _, name)), None) => name.clone(),
            (None, _) => "(no section)".to_string(),
        }
    }

    /// Rebuild the recent group: your opens (newest first), then agent-referenced
    /// items not already shown, in scan (recency) order. Capped at RECENT_CAP.
    fn rebuild_recent(&mut self) {
        let agent_set: HashSet<&str> = self.agent.iter().map(String::as_str).collect();
        let mut out: Vec<Item> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        // 1. your lens opens (functions), newest first
        for name in &self.opens {
            if !self.fn_names.contains(name) || !seen.insert(format!("f:{name}")) {
                continue;
            }
            let addr = self.fn_name_addr.get(name).cloned().unwrap_or_default();
            let mut src = YOU;
            if agent_set.contains(name.as_str()) || agent_set.contains(addr.as_str()) {
                src |= AGENT;
            }
            let disp = self
                .fn_display
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone());
            out.push(Item {
                auto: is_auto_name(&disp, &addr),
                addr,
                name: disp,
                target: name.clone(),
                import: self.imports.contains(name),
                annot: String::new(),
                src,
            });
        }

        // 2. agent-referenced tokens, in recency order
        for tok in &self.agent {
            if out.len() >= RECENT_CAP {
                break;
            }
            if let Some(target) = self.fn_alias_target.get(tok) {
                if !seen.insert(format!("f:{target}")) {
                    continue;
                }
                let addr = self.fn_name_addr.get(tok).cloned().unwrap_or_default();
                let disp = self
                    .fn_display
                    .get(target)
                    .cloned()
                    .unwrap_or_else(|| tok.clone());
                out.push(Item {
                    auto: is_auto_name(&disp, &addr),
                    addr,
                    name: disp,
                    target: target.clone(),
                    import: self.imports.contains(tok) || self.imports.contains(target),
                    annot: String::new(),
                    src: AGENT,
                });
            } else if tok.starts_with("0x") {
                if let Some(fname) = self.fn_addr_name.get(tok) {
                    if !seen.insert(format!("f:{fname}")) {
                        continue;
                    }
                    let disp = self
                        .fn_display
                        .get(fname)
                        .cloned()
                        .unwrap_or_else(|| fname.clone());
                    out.push(Item {
                        auto: is_auto_name(&disp, tok),
                        addr: tok.clone(),
                        name: disp,
                        target: fname.clone(),
                        import: self.imports.contains(fname),
                        annot: String::new(),
                        src: AGENT,
                    });
                } else if let Some(v) = parse_hex(tok) {
                    // A bare address that lands in no section of *this* target
                    // isn't ours — skip it rather than present a bogus row (a
                    // second guard behind the transcript provenance gate).
                    let in_section = self.ranges.iter().any(|(s, e, _)| v >= *s && v < *e);
                    if in_section && seen.insert(format!("a:{tok}")) {
                        out.push(Item {
                            addr: tok.clone(),
                            name: String::new(),
                            target: tok.clone(),
                            import: false,
                            auto: false,
                            annot: self.annotate(v),
                            src: AGENT,
                        });
                    }
                }
            }
            // a plain non-function identifier is noise — skip it
        }

        out.truncate(RECENT_CAP);
        self.recent = out;
    }

    fn meta(&self, i: usize) -> (u64, u32) {
        self.meta_by_addr
            .get(&self.all[i].addr)
            .copied()
            .unwrap_or((0, 0))
    }

    /// Indices into `all` that survive the row-class filter (`f`) and the search
    /// filter, in the active sort order (`o`).
    fn visible_all(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        let mut v: Vec<usize> = (0..self.all.len())
            .filter(|&i| {
                let it = &self.all[i];
                self.class.admits(it)
                    && (f.is_empty()
                        || it.name.to_lowercase().contains(&f)
                        || it.target.to_lowercase().contains(&f)
                        || it.addr.contains(&f))
            })
            .collect();
        match self.order {
            // Descending: the point of these orders is "show me the big ones".
            Order::Size => v.sort_by_key(|&i| std::cmp::Reverse(self.meta(i).0)),
            Order::Blocks => v.sort_by_key(|&i| std::cmp::Reverse(self.meta(i).1)),
            Order::Addr if !f.is_empty() => {
                // rank by match quality on the name so `main` beats `__libc_start_main`
                v.sort_by_key(|&i| {
                    let name = self.all[i].name.to_lowercase();
                    if name == f {
                        0
                    } else if name.starts_with(&f) {
                        1
                    } else if name.contains(&f) {
                        2
                    } else {
                        3
                    }
                });
            }
            Order::Addr => {}
        }
        v
    }

    /// Rule text for the section a run of rows belongs to. Address order *is*
    /// file layout, so these are real landmarks in a 6000-row scroll, not decor.
    /// The leading chevron is the fold state.
    fn sec_header(&self, s: usize, n: usize) -> String {
        let chev = if self.collapsed.contains(&s) {
            '▸'
        } else {
            '▾'
        };
        match self.ranges.get(s) {
            Some((st, en, name)) => format!("{chev} {name}   {st:#x} – {en:#x}   ·   {n} fn"),
            None => format!("{chev} (outside any section)   ·   {n} fn"),
        }
    }

    /// The current display rows: grouped (recent + per-section runs) in address
    /// order, or a flat ranked list when sorting or filtering.
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if self.filter.is_empty() && !self.recent.is_empty() {
            rows.push(Row::Header("recent  ·  ▸ you   ◆ agent   ★ both".into()));
            rows.extend((0..self.recent.len()).map(Row::Recent));
        }
        let idx = self.visible_all();
        if self.filter.is_empty() && self.order == Order::Addr {
            let mut counts: HashMap<usize, usize> = HashMap::new();
            for &i in &idx {
                *counts.entry(self.all_sec[i]).or_default() += 1;
            }
            let mut cur: Option<usize> = None;
            for &i in &idx {
                let s = self.all_sec[i];
                if cur != Some(s) {
                    cur = Some(s);
                    rows.push(Row::Section(s, self.sec_header(s, counts[&s])));
                }
                if !self.collapsed.contains(&s) {
                    rows.push(Row::All(i));
                }
            }
        } else {
            if self.filter.is_empty() {
                rows.push(Row::Header(format!(
                    "{} functions · {}",
                    self.class.label(),
                    self.order.label()
                )));
            }
            rows.extend(idx.into_iter().map(Row::All));
        }
        rows
    }

    /// Rows the cursor may land on. Section rules are included — they're the
    /// handle you fold with; only the recent group's legend is inert.
    fn sel_positions(rows: &[Row]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| !matches!(r, Row::Header(_)))
            .map(|(i, _)| i)
            .collect()
    }

    /// The section the cursor is on or inside, if the list is sectioned.
    fn sel_section(&self, rows: &[Row]) -> Option<usize> {
        match rows.get(self.sel)? {
            Row::Section(s, _) => Some(*s),
            Row::All(i) => Some(*self.all_sec.get(*i)?),
            _ => None,
        }
    }

    /// Fold or unfold `s`, then park the cursor on its rule — after a fold the
    /// rows you were standing in are gone, and silently landing somewhere else
    /// in the binary loses your place.
    fn toggle_section(&mut self, s: usize) {
        if !self.collapsed.insert(s) {
            self.collapsed.remove(&s);
        }
        let rows = self.rows();
        if let Some(i) = rows
            .iter()
            .position(|r| matches!(r, Row::Section(x, _) if *x == s))
        {
            self.sel = i;
            self.top = self.top.min(i);
        }
    }

    /// Fold every section — or unfold them all, but only once they *all* are.
    /// Folding is the useful direction (6000 rows become a table of contents in
    /// one screen), so a half-folded list folds the rest rather than springing
    /// back open; a second press is then the way back.
    fn toggle_all_sections(&mut self) {
        let secs: Vec<usize> = {
            let mut v: Vec<usize> = self
                .rows()
                .iter()
                .filter_map(|r| match r {
                    Row::Section(s, _) => Some(*s),
                    _ => None,
                })
                .collect();
            v.dedup();
            v
        };
        if !secs.is_empty() && secs.iter().all(|s| self.collapsed.contains(s)) {
            self.collapsed.clear();
        } else {
            self.collapsed.extend(secs);
        }
        self.sel = 0;
        self.top = 0;
    }

    /// Ensure `sel` points at a selectable row (never a header / out of range).
    fn snap(&mut self, rows: &[Row]) {
        let sp = Self::sel_positions(rows);
        if sp.is_empty() {
            self.sel = 0;
        } else if !sp.contains(&self.sel) {
            self.sel = *sp
                .iter()
                .find(|&&i| i >= self.sel)
                .unwrap_or(sp.last().unwrap());
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let rows = self.rows();
        let sp = Self::sel_positions(&rows);
        if sp.is_empty() {
            return;
        }
        let cur = sp.iter().position(|&i| i == self.sel).unwrap_or(0) as i64;
        let ncur = (cur + delta).clamp(0, sp.len() as i64 - 1) as usize;
        self.sel = sp[ncur];
    }

    fn current_target(&self) -> Option<String> {
        match self.rows().get(self.sel)? {
            Row::Recent(i) => Some(self.recent[*i].target.clone()),
            Row::All(i) => Some(self.all[*i].target.clone()),
            Row::Header(_) | Row::Section(..) => None,
        }
    }

    fn draw_item(
        &self,
        buf: &mut Buffer,
        x0: u16,
        y: u16,
        w: usize,
        it: &Item,
        is_sel: bool,
        recent: bool,
    ) {
        // Leading marker: recency in the recent group, a hop glyph for imports
        // so a PLT stub is recognisable as one at a glance instead of being a
        // dimmer copy of a real function.
        let glyph = match (recent, it.import) {
            (true, _) => match (it.src & YOU != 0, it.src & AGENT != 0) {
                (true, true) => "★ ",
                (true, false) => "▸ ",
                (false, true) => "◆ ",
                _ => "  ",
            },
            (false, true) => "↗ ",
            (false, false) => "  ",
        };
        // A `sub_<addr>` name is the address column repeated — render a marker
        // instead so the eye only lands on names that carry information. The
        // model keeps the real name, so search still matches `sub_…`.
        let tail = if it.name.is_empty() {
            if it.annot.is_empty() {
                "(addr)".to_string()
            } else {
                it.annot.clone()
            }
        } else if it.auto {
            auto_label(&it.name)
        } else {
            it.name.clone()
        };

        // `used` is the column where the name field begins: glyph (2) +
        // address (awidth) + a 2-space gutter.
        let used = 2 + self.awidth + 2;
        // Metrics are for real code only: import stubs are uniformly tiny (their
        // numbers are noise) and data rows have no body to measure.
        let metrics = if it.import || it.name.is_empty() {
            None
        } else {
            self.meta_by_addr.get(&it.addr).copied()
        };
        let (namew, barw, statw) = match metrics {
            Some(_) => columns(w.saturating_sub(used), self.namew_cap),
            None => (w.saturating_sub(used), 0, 0),
        };
        let name = truncate(&tail, namew);

        let mut spans = vec![
            Span::styled(glyph.to_string(), Style::default().fg(theme::MARK)),
            Span::styled(
                format!("{:<aw$}  ", it.addr, aw = self.awidth),
                Style::default().fg(theme::ADDR).add_modifier(if it.import {
                    Modifier::DIM
                } else {
                    Modifier::empty()
                }),
            ),
        ];
        let name_style = if it.name.is_empty() {
            Style::default().fg(Color::Magenta) // data annotation
        } else if it.auto || it.import {
            Style::default().fg(theme::NAME).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(theme::NAME)
        };
        spans.push(Span::styled(format!("{name:<namew$}"), name_style));
        if let Some((size, bbc)) = metrics {
            if barw > 0 {
                let bar = size_bar(size, barw);
                // One accent, spent on the only thing worth interrupting a scan
                // for: the branchiest tenth of the binary.
                let style = if bbc >= self.bb_hot {
                    Style::default().fg(theme::MARK)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                spans.push(Span::styled(format!(" {bar:<barw$} "), style));
            }
            if statw > 0 {
                spans.push(Span::styled(
                    stats_col(size, bbc),
                    Style::default().fg(theme::ADDR).add_modifier(Modifier::DIM),
                ));
            }
        }

        if is_sel {
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            crate::ui::put_str(
                buf,
                x0,
                y,
                format!("{text:<w$}"),
                w,
                Style::default().add_modifier(Modifier::REVERSED),
            );
            return;
        }
        crate::ui::put_spans(buf, x0, y, w, &spans);
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &Ctx) {
        let rows = self.rows();
        self.snap(&rows);
        let listh = area.height.saturating_sub(3) as usize;
        if self.sel < self.top {
            self.top = self.sel;
        }
        if listh > 0 && self.sel >= self.top + listh {
            self.top = self.sel + 1 - listh;
        }

        let x0 = area.x;
        let w = area.width as usize;
        // Counted from the filter, not the rows: folding a section hides lines
        // but doesn't change how many symbols match.
        let shown = self.visible_all().len();
        // View-state is its own group: a ` · ` boundary, then the count.
        let mut bar = crate::ui::crumbs(ctx);
        bar.push(crate::ui::crumb_sep());
        bar.push(Span::styled(
            format!("symbols  {}/{}", shown, self.all.len()),
            Style::default().add_modifier(Modifier::DIM),
        ));
        // Surface a non-default class/order in the header — a filtered list that
        // looks like the whole list is a lie about what you're looking at.
        if self.class != Class::All || self.order != Order::Addr {
            bar.push(crate::ui::crumb_sep());
            bar.push(Span::styled(
                format!("{} · {}", self.class.label(), self.order.label()),
                Style::default().fg(theme::MARK),
            ));
        }
        crate::ui::render_bar(buf, x0, area.y, w, &bar);
        let state = match self.mode {
            Mode::Search => format!(" /{}", self.filter),
            Mode::Normal if !self.filter.is_empty() => format!(" filter: {}", self.filter),
            Mode::Normal => String::new(),
        };
        let hint = match self.mode {
            Mode::Search => crate::ui::hint_bar(&[
                &[("type", ""), ("↑↓", "pick")],
                &[("Enter", "open"), ("Tab", "list"), ("Esc", "cancel")],
                &[("?", "help")],
            ]),
            Mode::Normal => crate::ui::hint_bar(&[
                &[("j/k", "move"), ("/", "search")],
                &[("Enter", "open/fold"), ("z/Z", "fold"), ("x", "xrefs")],
                &[("f", "class"), ("o", "order")],
                &[("m", "menu"), ("v", "list"), ("i", "switch")],
                &[("?", "help"), ("q", "quit")],
            ]),
        };
        crate::ui::put_str(
            buf,
            x0,
            area.y + 1,
            state,
            w,
            Style::default().add_modifier(Modifier::DIM),
        );
        crate::ui::render_bar(buf, x0, area.y + area.height.saturating_sub(1), w, &hint);

        for (ri, row) in rows.iter().enumerate().skip(self.top).take(listh) {
            let y = area.y + 2 + (ri - self.top) as u16;
            match row {
                Row::Header(h) => {
                    let label = format!("── {h} ");
                    let pad = w.saturating_sub(label.chars().count());
                    let line = format!("{label}{}", "─".repeat(pad));
                    crate::ui::put_str(
                        buf,
                        x0,
                        y,
                        line,
                        w,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    );
                }
                Row::Section(_, h) => {
                    let label = format!("── {h} ");
                    let pad = w.saturating_sub(label.chars().count());
                    let line = format!("{label}{}", "─".repeat(pad));
                    let style = if ri == self.sel {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM)
                    };
                    crate::ui::put_str(buf, x0, y, line, w, style);
                }
                Row::Recent(i) => {
                    self.draw_item(buf, x0, y, w, &self.recent[*i], ri == self.sel, true)
                }
                Row::All(i) => self.draw_item(buf, x0, y, w, &self.all[*i], ri == self.sel, false),
            }
        }

        // section overlay on top of the list
        if let Some(off) = self.sec_off {
            let bw = (area.width.saturating_sub(6)).clamp(50, 100);
            let bh = (area.height.saturating_sub(4)).clamp(8, 30);
            let bx = area.x + (area.width.saturating_sub(bw)) / 2;
            let by = area.y + (area.height.saturating_sub(bh)) / 2;
            crate::ui::draw_box(
                buf,
                bx,
                by,
                bw,
                bh,
                "sections  ·  r-x=exec  rw-=data  w+x flagged",
            );
            let view_h = (bh as usize).saturating_sub(3);
            for (i, ln) in self.sec_lines.iter().skip(off).take(view_h).enumerate() {
                crate::ui::put_str(
                    buf,
                    bx + 2,
                    by + 1 + i as u16,
                    ln,
                    (bw - 4) as usize,
                    Style::default().fg(Color::Yellow),
                );
            }
            crate::ui::put_str(
                buf,
                bx + 2,
                by + bh - 1,
                " j/k scroll · ? help · s/q close ",
                (bw - 4) as usize,
                Style::default().add_modifier(Modifier::DIM),
            );
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent, area: Rect) {
        if self.sec_off.is_some() {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.move_sel(-20),
            MouseEventKind::ScrollDown => self.move_sel(20),
            MouseEventKind::Down(_) => {
                let rows = self.rows();
                let ri = self.top + m.row.saturating_sub(area.y + 2) as usize;
                match rows.get(ri) {
                    Some(Row::Recent(_)) | Some(Row::All(_)) => self.sel = ri,
                    // A section rule has no other purpose — clicking it folds.
                    Some(Row::Section(s, _)) => {
                        let s = *s;
                        self.sel = ri;
                        self.toggle_section(s);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Action {
        // section overlay captures keys while open
        if let Some(off) = self.sec_off {
            let n = self.sec_lines.len();
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('s') | KeyCode::Enter => {
                    self.sec_off = None
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.sec_off = Some((off + 1).min(n.saturating_sub(1)))
                }
                KeyCode::Char('k') | KeyCode::Up => self.sec_off = Some(off.saturating_sub(1)),
                KeyCode::PageDown => self.sec_off = Some((off + 10).min(n.saturating_sub(1))),
                KeyCode::PageUp => self.sec_off = Some(off.saturating_sub(10)),
                _ => {}
            }
            return Action::None;
        }

        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Mode::Search = self.mode {
            match k.code {
                // Enter opens the highlighted (top-ranked) match directly.
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    if let Some(t) = self.current_target() {
                        return Action::OpenDecompile(t);
                    }
                }
                // Tab keeps the filter but drops to normal mode.
                KeyCode::Tab => self.mode = Mode::Normal,
                KeyCode::Esc => {
                    self.filter = self.prev_filter.clone();
                    self.mode = Mode::Normal;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.sel = 0;
                }
                KeyCode::Down => self.move_sel(1),
                KeyCode::Up => self.move_sel(-1),
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.sel = 0;
                }
                _ => {}
            }
            return Action::None;
        }

        // normal mode
        if self.pending_g {
            self.pending_g = false;
            if k.code == KeyCode::Char('g') {
                self.move_sel(i64::MIN / 2);
                return Action::None;
            }
        }
        match k.code {
            // q is the only quit. Esc never closes the pane: it clears the
            // filter if set, else no-ops (Symbols is already "home").
            KeyCode::Char('q') => return Action::Quit,
            // Esc peels back one layer of "what am I looking at": the search
            // filter first, then the class/order lenses, then nothing.
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.sel = 0;
                    self.top = 0;
                } else if self.class != Class::All || self.order != Order::Addr {
                    self.class = Class::All;
                    self.order = Order::Addr;
                    self.sel = 0;
                    self.top = 0;
                }
            }
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('G') => self.move_sel(i64::MAX / 2),
            KeyCode::Char('d') if ctrl => self.move_sel(10),
            KeyCode::Char('u') if ctrl => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(20),
            KeyCode::PageUp => self.move_sel(-20),
            KeyCode::Char('/') => {
                self.prev_filter = self.filter.clone();
                self.filter.clear();
                self.mode = Mode::Search;
                self.sel = 0;
            }
            // Enter/Space on a section rule folds it; on a function it opens.
            KeyCode::Enter | KeyCode::Char(' ') => {
                let rows = self.rows();
                if let Some(Row::Section(s, _)) = rows.get(self.sel) {
                    let s = *s;
                    self.toggle_section(s);
                } else if let Some(t) = self.current_target() {
                    return Action::OpenDecompile(t);
                }
            }
            // `z` folds the section you're standing in, without walking back up
            // to its rule; `Z` folds every section into a table of contents.
            KeyCode::Char('z') => {
                let rows = self.rows();
                if let Some(s) = self.sel_section(&rows) {
                    self.toggle_section(s);
                }
            }
            KeyCode::Char('Z') => self.toggle_all_sections(),
            KeyCode::Char('x') => {
                if let Some(t) = self.current_target() {
                    return Action::OpenXrefs(t);
                }
            }
            KeyCode::Char('s') => self.sec_off = Some(0),
            KeyCode::Char('f') => {
                self.class = self.class.next();
                self.sel = 0;
                self.top = 0;
            }
            KeyCode::Char('o') => {
                self.order = self.order.next();
                self.sel = 0;
                self.top = 0;
            }
            KeyCode::Char('i') => return Action::Switch,
            _ => {}
        }
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_size_scales_units() {
        assert_eq!(fmt_size(0), "0");
        assert_eq!(fmt_size(20), "20");
        assert_eq!(fmt_size(1023), "1023");
        assert_eq!(fmt_size(1024), "1.0K");
        assert_eq!(fmt_size(1536), "1.5K");
        assert_eq!(fmt_size(1024 * 1024), "1.0M");
    }

    #[test]
    fn stats_col_is_fixed_width() {
        // Column width must match STATS_W so the name field math lines up.
        assert_eq!(stats_col(20, 1).chars().count(), STATS_W);
        assert_eq!(stats_col(4096, 23).chars().count(), STATS_W);
        assert_eq!(stats_col(1024 * 1024, 999).chars().count(), STATS_W);
        // Four-digit block counts are real (a large stripped daemon hits them)
        // and must not push the ` bb` suffix out of the column.
        assert_eq!(stats_col(28 * 1024, 1387).chars().count(), STATS_W);
        assert_eq!(stats_col(28 * 1024, 9999).chars().count(), STATS_W);
    }

    #[test]
    fn auto_names_are_the_address_repeated() {
        assert!(is_auto_name("sub_41c3a0", "0x41c3a0"));
        // leading zeros / case differences still name the same address
        assert!(is_auto_name("sub_0041c3a0", "0x41c3a0"));
        // a real name, even one that merely starts with sub_
        assert!(!is_auto_name("sub_parse_header", "0x41c3a0"));
        // a sub_ name pointing somewhere else is information — keep it
        assert!(!is_auto_name("sub_41c3a0", "0x41d280"));
        assert!(!is_auto_name("parse_frame_header", "0x41c3a0"));
        assert!(!is_auto_name("sub_41c3a0", "not-an-addr"));
        // a thunk into an unnamed function is a placeholder wherever it lives
        assert!(is_auto_name("j_sub_4a7ee0", "0x456830"));
        // ...but a thunk into a *named* one is recovered information
        assert!(!is_auto_name("j_memcpy", "0x456830"));
    }

    #[test]
    fn placeholder_names_collapse_but_thunks_keep_their_target() {
        assert_eq!(auto_label("sub_41c3a0"), NO_NAME);
        assert_eq!(auto_label("j_sub_4a7ee0"), "→ 0x4a7ee0");
    }

    #[test]
    fn size_bar_is_monotone_and_bounded() {
        let w = 10;
        // Every function gets at least a sliver, nothing overflows the field.
        for n in [1u64, 16, 100, 4096, 65536, 1 << 24] {
            let len = size_bar(n, w).chars().count();
            assert!((1..=w).contains(&len), "size {n} -> {len} cells");
        }
        // Bigger bodies never draw a shorter bar.
        let mut prev = 0;
        for n in [1u64, 64, 512, 4096, 32768, 1 << 20] {
            let len = size_bar(n, w).chars().count();
            assert!(len >= prev, "size {n} shrank the bar");
            prev = len;
        }
        assert_eq!(size_bar(1 << 20, w).chars().count(), w); // saturates
        assert_eq!(size_bar(4096, 0), "");
    }

    #[test]
    fn columns_pack_left_and_cap_the_name() {
        // Wide pane: the name stops at the cap instead of stranding the metrics.
        let (n, b, s) = columns(200, NAME_MAX);
        assert_eq!((n, b, s), (NAME_MAX, BAR_W, STATS_W));
        // A target with only short names pulls the metrics in further still.
        assert_eq!(columns(200, 14), (14, BAR_W, STATS_W));
        // Medium: bar and stats still fit, name takes what's left.
        let (n, b, s) = columns(50, NAME_MAX);
        assert_eq!((n, b, s), (50 - 2 - BAR_W - STATS_W, BAR_W, STATS_W));
        assert_eq!(n + b + s + 2, 50); // exactly fills the row
                                       // Narrow: the bar goes first, then the stats.
        let (_, b, s) = columns(30, NAME_MAX);
        assert_eq!((b, s), (0, STATS_W));
        let (n, b, s) = columns(20, NAME_MAX);
        assert_eq!((n, b, s), (20, 0, 0));
    }

    fn item(name: &str, auto: bool, import: bool) -> Item {
        Item {
            addr: "0x1000".into(),
            name: name.into(),
            target: name.into(),
            import,
            auto,
            annot: String::new(),
            src: 0,
        }
    }

    #[test]
    fn name_cap_sizes_to_the_names_that_exist() {
        // Sized to the longest recovered name...
        let all = vec![
            item("parse_frame_header", false, false),
            item("sub_1000", true, false),
            item("main", false, false),
        ];
        assert_eq!(name_cap(&all), "parse_frame_header".len());
        // ...ignoring placeholders and imports, which don't need the width.
        let stripped = vec![
            item("sub_1000", true, false),
            item("a_very_long_imported_symbol_name", false, true),
        ];
        assert_eq!(name_cap(&stripped), NAME_MIN);
        // ...and never past the cap.
        let verbose = vec![item(&"x".repeat(200), false, false)];
        assert_eq!(name_cap(&verbose), NAME_MAX);
        assert_eq!(name_cap(&[]), NAME_MIN);
    }

    #[test]
    fn truncate_ellipsizes_when_over() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactlyten", 10), "exactlyten");
        assert_eq!(truncate("toolonganame", 6), "toolo…");
        assert_eq!(truncate("x", 0), "");
        // Result never exceeds the cap.
        assert!(truncate("aaaaaaaaaa", 4).chars().count() <= 4);
    }
}
