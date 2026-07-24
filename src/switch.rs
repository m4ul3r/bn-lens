//! Instance/target switcher — a ranger-style miller-columns modal:
//!   [ instances ] [ targets of the highlighted instance ] [ target info ]
//! Move within a column with j/k, between columns with h/l (or ←/→/Tab),
//! Enter to re-point the lens at the highlighted instance+target.

use crate::bn::{Bn, Instance, TargetItem};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

pub enum Outcome {
    Continue,
    Cancel,
    Apply(String, String), // (instance, target selector)
}

#[derive(PartialEq)]
enum Focus {
    Instances,
    Targets,
}

/// An in-flight `bn` read for one of the columns, delivered by [`Switcher::poll`].
/// Every read the switcher needs — the instance list, an instance's targets, a
/// target's info — is a socket or CLI round trip, and running them inline froze
/// the modal as it opened and again on every `j`/`k`.
enum Fetch {
    Sessions(Receiver<Vec<Instance>>),
    Targets {
        id: String,
        rx: Receiver<Vec<TargetItem>>,
    },
    Info {
        key: String,
        rx: Receiver<Vec<String>>,
    },
}

pub struct Switcher {
    bn_bin: String,
    cur_instance: String,
    instances: Vec<Instance>,
    inst_sel: usize,
    targets: Vec<TargetItem>,
    tgt_sel: usize,
    focus: Focus,
    preview: Vec<String>,
    tcache: std::collections::HashMap<String, Vec<TargetItem>>,
    icache: std::collections::HashMap<String, Vec<String>>,
    /// The instance `targets` belongs to, and the `<instance>\0<selector>` key
    /// `preview` belongs to — so [`Switcher::pump`] can tell an up-to-date column
    /// from one the cursor has moved off.
    targets_of: String,
    preview_of: String,
    /// At most one read in flight at a time. Serialising them is deliberate:
    /// holding `j` down a dozen instances then costs one `bn` read for the row
    /// you land on, not one per row you pass over.
    fetch: Option<Fetch>,
    /// Type-ahead filter over the *focused* column (instances or targets).
    filter: String,
    searching: bool,
}

impl Switcher {
    pub fn new(bn_bin: &str, cur_instance: &str) -> Self {
        let mut s = Switcher {
            bn_bin: bn_bin.to_string(),
            cur_instance: cur_instance.to_string(),
            instances: Vec::new(),
            inst_sel: 0,
            targets: Vec::new(),
            tgt_sel: 0,
            focus: Focus::Instances,
            preview: Vec::new(),
            tcache: std::collections::HashMap::new(),
            icache: std::collections::HashMap::new(),
            targets_of: String::new(),
            preview_of: String::new(),
            fetch: None,
            filter: String::new(),
            searching: false,
        };
        s.start_sessions();
        s
    }

    /// True while a `bn` read is outstanding — the modal shows a reading marker
    /// and the event loop ticks faster so a delivered column appears promptly.
    pub fn pending(&self) -> bool {
        self.fetch.is_some()
    }

    /// True while the type-ahead filter is capturing raw text (so App must not
    /// steal `?`/`m` as shortcuts — they belong in the query).
    pub fn is_searching(&self) -> bool {
        self.searching
    }

    /// The filter applies only to the currently-focused column.
    fn inst_filter_on(&self) -> bool {
        self.focus == Focus::Instances && !self.filter.is_empty()
    }
    fn tgt_filter_on(&self) -> bool {
        self.focus == Focus::Targets && !self.filter.is_empty()
    }

    /// Instance indices passing the (focus-gated) filter, in list order.
    fn visible_insts(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.instances.len())
            .filter(|&i| {
                !self.inst_filter_on() || self.instances[i].instance_id.to_lowercase().contains(&f)
            })
            .collect()
    }

    /// Target indices passing the (focus-gated) filter, in list order.
    fn visible_tgts(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.targets.len())
            .filter(|&i| {
                let t = &self.targets[i];
                !self.tgt_filter_on()
                    || t.selector.to_lowercase().contains(&f)
                    || ui::clean_target_label(&t.selector)
                        .to_lowercase()
                        .contains(&f)
            })
            .collect()
    }

    /// Move the instance cursor by `delta` over the visible (filtered) set.
    fn move_inst(&mut self, delta: i64) {
        let vis = self.visible_insts();
        if vis.is_empty() {
            return;
        }
        let cur = vis.iter().position(|&i| i == self.inst_sel).unwrap_or(0) as i64;
        let new = vis[(cur + delta).clamp(0, vis.len() as i64 - 1) as usize];
        if new != self.inst_sel {
            self.inst_sel = new;
            self.pump();
        }
    }

    /// Move the target cursor by `delta` over the visible (filtered) set.
    fn move_tgt(&mut self, delta: i64) {
        let vis = self.visible_tgts();
        if vis.is_empty() {
            return;
        }
        let cur = vis.iter().position(|&i| i == self.tgt_sel).unwrap_or(0) as i64;
        let new = vis[(cur + delta).clamp(0, vis.len() as i64 - 1) as usize];
        if new != self.tgt_sel {
            self.tgt_sel = new;
            self.pump();
        }
    }

    /// After the filter changes, pull the focused cursor onto the first visible
    /// row if it fell outside the narrowed set.
    fn snap_focused(&mut self) {
        match self.focus {
            Focus::Instances => {
                let vis = self.visible_insts();
                if !vis.contains(&self.inst_sel) {
                    if let Some(&first) = vis.first() {
                        self.inst_sel = first;
                        self.pump();
                    }
                }
            }
            Focus::Targets => {
                let vis = self.visible_tgts();
                if !vis.contains(&self.tgt_sel) {
                    if let Some(&first) = vis.first() {
                        self.tgt_sel = first;
                        self.pump();
                    }
                }
            }
        }
    }

    fn cur_inst_id(&self) -> Option<String> {
        self.instances
            .get(self.inst_sel)
            .map(|i| i.instance_id.clone())
    }

    fn start_sessions(&mut self) {
        let (tx, rx) = channel();
        let bn_bin = self.bn_bin.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Bn::session_list(&bn_bin));
        });
        self.fetch = Some(Fetch::Sessions(rx));
    }

    fn start_targets(&mut self, id: String) {
        let (tx, rx) = channel();
        let bn_bin = self.bn_bin.clone();
        let inst = id.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Bn::new(bn_bin, Some(inst), None).target_list());
        });
        self.fetch = Some(Fetch::Targets { id, rx });
    }

    fn start_info(&mut self, id: String, selector: String, key: String) {
        let (tx, rx) = channel();
        let bn_bin = self.bn_bin.clone();
        std::thread::spawn(move || {
            let raw = Bn::new(bn_bin, Some(id), Some(selector)).target_info_raw();
            let _ = tx.send(format_info(&raw));
        });
        self.fetch = Some(Fetch::Info { key, rx });
    }

    /// Reconcile the target/info columns with the cursor: adopt anything already
    /// cached, and start one worker read for whatever is still missing. Never
    /// blocks, and never starts a second read while one is in flight — the next
    /// [`Self::poll`] re-runs this and picks up wherever the cursor now is.
    fn pump(&mut self) {
        if matches!(self.fetch, Some(Fetch::Sessions(_))) {
            return; // nothing to reconcile until the instance list lands
        }
        let Some(id) = self.cur_inst_id() else {
            self.targets.clear();
            self.targets_of.clear();
            self.preview.clear();
            self.preview_of.clear();
            return;
        };
        if self.targets_of != id {
            self.preview.clear();
            self.preview_of.clear();
            match self.tcache.get(&id).cloned() {
                Some(list) => {
                    self.targets = list;
                    self.targets_of = id.clone();
                    // Land on the first row the filter actually shows: row 0 can
                    // be hidden (a filter typed while this list was still empty
                    // narrowed nothing), and a hidden cursor means Enter opens a
                    // target the user cannot see.
                    self.tgt_sel = self.visible_tgts().first().copied().unwrap_or(0);
                }
                None => {
                    self.targets.clear();
                    if self.fetch.is_none() {
                        self.start_targets(id);
                    }
                    return;
                }
            }
        }
        let Some(selector) = self.targets.get(self.tgt_sel).map(|t| t.selector.clone()) else {
            self.preview.clear();
            self.preview_of.clear();
            return;
        };
        let key = format!("{id}\0{selector}");
        if self.preview_of == key {
            return;
        }
        match self.icache.get(&key) {
            Some(lines) => {
                self.preview = lines.clone();
                self.preview_of = key;
            }
            None => {
                self.preview.clear();
                if self.fetch.is_none() {
                    self.start_info(id, selector, key);
                }
            }
        }
    }

    /// Deliver a finished read (if any) and start the next one the cursor needs.
    /// Called once per event-loop tick; cheap when nothing is outstanding.
    pub fn poll(&mut self) {
        enum Done {
            Sessions(Vec<Instance>),
            Targets(String, Vec<TargetItem>),
            Info(String, Vec<String>),
        }
        // A dead worker still resolves — an empty list / an unavailable note is
        // cached so `pump` doesn't respawn the same read every tick.
        let done = match &self.fetch {
            None => None,
            Some(Fetch::Sessions(rx)) => match rx.try_recv() {
                Ok(list) => Some(Done::Sessions(list)),
                Err(TryRecvError::Disconnected) => Some(Done::Sessions(Vec::new())),
                Err(TryRecvError::Empty) => None,
            },
            Some(Fetch::Targets { id, rx }) => match rx.try_recv() {
                Ok(list) => Some(Done::Targets(id.clone(), list)),
                Err(TryRecvError::Disconnected) => Some(Done::Targets(id.clone(), Vec::new())),
                Err(TryRecvError::Empty) => None,
            },
            Some(Fetch::Info { key, rx }) => match rx.try_recv() {
                Ok(lines) => Some(Done::Info(key.clone(), lines)),
                Err(TryRecvError::Disconnected) => {
                    Some(Done::Info(key.clone(), vec!["(info unavailable)".into()]))
                }
                Err(TryRecvError::Empty) => None,
            },
        };
        match done {
            None => return,
            Some(Done::Sessions(list)) => {
                self.inst_sel = list
                    .iter()
                    .position(|i| i.instance_id == self.cur_instance)
                    .unwrap_or(0);
                self.instances = list;
                self.fetch = None;
                // The modal accepts `/` input while this read is outstanding, and
                // `snap_focused` was a no-op against the empty list. The cursor
                // (here, `cur_instance`) can therefore sit on a row the filter
                // hides — reconcile before `pump` starts reading targets for it,
                // or the visible rows show no selection and Enter switches to an
                // off-screen instance.
                self.snap_focused();
            }
            Some(Done::Targets(id, list)) => {
                self.tcache.insert(id, list);
                self.fetch = None;
            }
            Some(Done::Info(key, lines)) => {
                self.icache.insert(key, lines);
                self.fetch = None;
            }
        }
        self.pump();
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Outcome {
        // Type-ahead filter mode: characters narrow the focused column.
        if self.searching {
            match k.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.searching = false;
                }
                // Enter/Tab commit the filter (leave search mode, keep the
                // narrowed list) so you can then j/k and Enter to select.
                KeyCode::Enter | KeyCode::Tab => self.searching = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.snap_focused();
                }
                KeyCode::Down => self.move_focused(1),
                KeyCode::Up => self.move_focused(-1),
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.snap_focused();
                }
                _ => {}
            }
            return Outcome::Continue;
        }

        match k.code {
            // Esc clears an active filter first; only then cancels the switcher.
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    return Outcome::Cancel;
                }
                self.filter.clear();
                self.snap_focused();
            }
            KeyCode::Char('q') => return Outcome::Cancel,
            KeyCode::Char('/') => {
                self.filter.clear();
                self.searching = true;
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_focused(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_focused(-1),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                if !self.targets.is_empty() {
                    self.focus = Focus::Targets;
                    self.filter.clear(); // filter is per-column
                }
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => {
                self.focus = Focus::Instances;
                self.filter.clear();
            }
            KeyCode::Enter => {
                if let (Some(id), Some(t)) = (self.cur_inst_id(), self.targets.get(self.tgt_sel)) {
                    return Outcome::Apply(id, t.selector.clone());
                }
            }
            _ => {}
        }
        Outcome::Continue
    }

    /// Placeholder row for an empty column: tell "still reading" apart from
    /// "nothing there" and from "the filter excluded everything".
    fn empty_row(&self, unloaded: bool, nothing: &str) -> String {
        if !unloaded {
            "(no match)".to_string()
        } else if self.pending() {
            "(reading bn…)".to_string()
        } else {
            nothing.to_string()
        }
    }

    fn move_focused(&mut self, delta: i64) {
        match self.focus {
            Focus::Instances => self.move_inst(delta),
            Focus::Targets => self.move_tgt(delta),
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return; // nothing can be drawn — and the column maths needs a body
        }
        // clear the whole area
        for y in area.y..area.y + area.height {
            crate::ui::put_str(
                buf,
                area.x,
                y,
                " ".repeat(area.width as usize),
                area.width as usize,
                Style::default(),
            );
        }
        let w = area.width;
        // The reads run on a worker thread, so say so rather than letting a
        // column sit mysteriously empty.
        let busy = if self.pending() { " ⟳" } else { "" };
        let header = if self.searching {
            format!(
                " switch bn{busy} — /{}   (Enter keep · Esc clear)",
                self.filter
            )
        } else if !self.filter.is_empty() {
            format!(
                " switch bn{busy} — filter: {}   j/k move · h/l cols · / filter · Enter select · Esc clear",
                self.filter
            )
        } else {
            format!(
                " switch bn{busy} — j/k move · h/l cols · / filter · Enter select · ? help · Esc cancel"
            )
        };
        ui::render_bar(
            buf,
            area.x,
            area.y,
            w as usize,
            &[ratatui::text::Span::styled(
                header,
                Style::default().add_modifier(Modifier::BOLD),
            )],
        );

        let body_y = area.y + 1;
        let body_h = area.height.saturating_sub(2) as usize; // header + (bottom margin)
        let c1w = 22u16.min(w / 3);
        let rem = w.saturating_sub(c1w + 2);
        // u32 for the ratio so a wide pane can't overflow the multiply; any of
        // c1w/c2w/c3w can still come out 0 on a very narrow pane, which the
        // column renderers below treat as "no room" instead of underflowing.
        let c2w = (rem as u32 * 6 / 10).min(46) as u16;
        let c3w = w.saturating_sub(c1w + c2w + 2);
        let c1x = area.x;
        let c2x = c1x + c1w + 1;
        let c3x = c2x + c2w + 1;

        // column separators
        for y in body_y..body_y + body_h as u16 {
            crate::ui::put_str(
                buf,
                c2x - 1,
                y,
                "│",
                1,
                Style::default().add_modifier(Modifier::DIM),
            );
            crate::ui::put_str(
                buf,
                c3x - 1,
                y,
                "│",
                1,
                Style::default().add_modifier(Modifier::DIM),
            );
        }

        // column 1 — instances (filtered when focus is here)
        let vis_i = self.visible_insts();
        let inst_rows: Vec<(String, Style)> = if vis_i.is_empty() {
            vec![(
                self.empty_row(self.instances.is_empty(), "(no live instances)"),
                Style::default().add_modifier(Modifier::DIM),
            )]
        } else {
            vis_i
                .iter()
                .map(|&idx| {
                    let i = &self.instances[idx];
                    let s = if i.instance_id == self.cur_instance {
                        Style::default().fg(Color::Green)
                    } else if i.binaries.is_empty() {
                        Style::default().add_modifier(Modifier::DIM)
                    } else {
                        Style::default()
                    };
                    (i.instance_id.clone(), s)
                })
                .collect()
        };
        let inst_pos = vis_i
            .iter()
            .position(|&i| i == self.inst_sel)
            .unwrap_or(usize::MAX);
        render_col(
            buf,
            c1x,
            body_y,
            c1w,
            body_h,
            "INSTANCES",
            &inst_rows,
            inst_pos,
            self.focus == Focus::Instances,
        );

        // column 2 — targets (placeholder row when the instance has none open,
        // or when a filter matched nothing)
        let vis_t = self.visible_tgts();
        let (tgt_rows, tgt_pos): (Vec<(String, Style)>, usize) = if self.targets.is_empty() {
            (
                vec![(
                    self.empty_row(true, "(no targets open)"),
                    Style::default().add_modifier(Modifier::DIM),
                )],
                usize::MAX, // never highlight the placeholder
            )
        } else if vis_t.is_empty() {
            (
                vec![(
                    "(no match)".to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                )],
                usize::MAX,
            )
        } else {
            (
                vis_t
                    .iter()
                    .map(|&idx| {
                        let t = &self.targets[idx];
                        let analysis = match t.analysis_state.as_str() {
                            "full" => "",
                            "quick" => "[Q] ",
                            _ => "[?] ",
                        };
                        (
                            format!(
                                "{}{}{}",
                                if t.active { "● " } else { "  " },
                                analysis,
                                ui::clean_target_label(&t.selector)
                            ),
                            Style::default(),
                        )
                    })
                    .collect(),
                vis_t
                    .iter()
                    .position(|&i| i == self.tgt_sel)
                    .unwrap_or(usize::MAX),
            )
        };
        render_col(
            buf,
            c2x,
            body_y,
            c2w,
            body_h,
            "TARGETS",
            &tgt_rows,
            tgt_pos,
            self.focus == Focus::Targets,
        );

        // column 3 — target info preview. A pane this narrow (`c3w` computes to 0
        // at ~4 columns) leaves it no room at all: skip it rather than underflow
        // the text width to ~65535.
        if c3w == 0 {
            return;
        }
        let info_w = c3w.saturating_sub(1) as usize;
        crate::ui::put_str(
            buf,
            c3x,
            body_y,
            " INFO",
            c3w as usize,
            Style::default().add_modifier(Modifier::DIM),
        );
        if self.preview.is_empty() {
            crate::ui::put_str(
                buf,
                c3x + 1,
                body_y + 1,
                self.empty_row(true, "(no target selected)"),
                info_w,
                Style::default().add_modifier(Modifier::DIM),
            );
        }
        for (i, line) in self
            .preview
            .iter()
            .take(body_h.saturating_sub(1))
            .enumerate()
        {
            crate::ui::put_str(
                buf,
                c3x + 1,
                body_y + 1 + i as u16,
                line,
                info_w,
                Style::default().fg(Color::Cyan),
            );
        }
    }
}

fn render_col(
    buf: &mut Buffer,
    x: u16,
    top_y: u16,
    w: u16,
    h: usize,
    header: &str,
    rows: &[(String, Style)],
    sel: usize,
    focused: bool,
) {
    // A pane narrow enough to compute a zero-width column has no room for this
    // one at all — bail instead of underflowing the label width below.
    if w == 0 || h == 0 {
        return;
    }
    crate::ui::put_str(
        buf,
        x,
        top_y,
        format!(" {header}"),
        w as usize,
        Style::default().add_modifier(Modifier::DIM),
    );
    let view_h = h.saturating_sub(1);
    // guard sel that is out of range (usize::MAX = "highlight nothing") so the
    // scroll offset can't underflow and skip every row.
    let vtop = if sel < rows.len() && sel >= view_h {
        sel + 1 - view_h
    } else {
        0
    };
    for (i, (label, base)) in rows.iter().skip(vtop).take(view_h).enumerate() {
        let idx = vtop + i;
        let y = top_y + 1 + i as u16;
        let style = if idx == sel {
            if focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                base.add_modifier(Modifier::BOLD).fg(Color::Cyan)
            }
        } else {
            *base
        };
        crate::ui::put_str(
            buf,
            x,
            y,
            format!(" {}", trunc(label, w.saturating_sub(1) as usize)),
            w as usize,
            style,
        );
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn format_info(raw: &str) -> Vec<String> {
    let keys = [
        "kind:",
        "arch:",
        "platform:",
        "image base:",
        "entry:",
        "analysis:",
        "functions:",
        "file:",
    ];
    let mut out = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if keys.iter().any(|k| t.starts_with(k)) {
            out.push(t.to_string());
        }
    }
    if out.is_empty() {
        out.push("(no info)".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{format_info, Focus, Switcher};
    use crate::bn::{Instance, TargetItem};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    /// A populated switcher with no worker attached — `Switcher::new` would spawn
    /// a `bn` read, and these tests are about geometry only.
    fn switcher() -> Switcher {
        Switcher {
            bn_bin: "bn".into(),
            cur_instance: "inst-a".into(),
            instances: vec![
                Instance {
                    instance_id: "inst-a".into(),
                    binaries: vec!["sample-daemon".into()],
                    started_at: "2026-01-01T00:00:00Z".into(),
                },
                Instance {
                    instance_id: "inst-b".into(),
                    binaries: Vec::new(),
                    started_at: "2026-01-01T00:00:00Z".into(),
                },
            ],
            inst_sel: 0,
            targets: vec![TargetItem {
                selector: "sample-daemon".into(),
                active: true,
                analysis_state: "full".into(),
            }],
            tgt_sel: 0,
            focus: Focus::Instances,
            preview: vec!["arch: aarch64".into(), "functions: 412".into()],
            tcache: std::collections::HashMap::new(),
            icache: std::collections::HashMap::new(),
            targets_of: "inst-a".into(),
            preview_of: "inst-a\0sample-daemon".into(),
            fetch: None,
            filter: String::new(),
            searching: false,
        }
    }

    fn render_at(width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        switcher().render(area, &mut buf);
        buf
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area().width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
    }

    #[test]
    fn renders_into_a_narrow_pane_without_underflowing() {
        // Widths 3 and 4 are the two documented column-maths underflows: w=3
        // computes c3w = 0 (the INFO text width went to ~65535), and w=4
        // computes c2w = 0, calling render_col with a zero width. Both panic in
        // debug and render garbage widths in release. Every write here also has
        // to stay inside the buffer, or ratatui panics on the out-of-bounds cell.
        for w in [0u16, 1, 2, 3, 4, 5, 8, 12, 25, 40] {
            let buf = render_at(w, 10);
            assert_eq!(buf.area().width, w, "width {w}");
        }
        // A short pane (no body rows) is fine too.
        for h in [0u16, 1, 2, 3] {
            render_at(25, h);
        }
    }

    #[test]
    fn a_wide_pane_still_draws_all_three_columns() {
        // Guards the narrow-pane early-returns against swallowing the normal case.
        let buf = render_at(100, 10);
        let headers = row(&buf, 1);
        assert!(headers.contains("INSTANCES"), "{headers:?}");
        assert!(headers.contains("TARGETS"), "{headers:?}");
        assert!(headers.contains("INFO"), "{headers:?}");
        assert!(row(&buf, 2).contains("inst-a"));
        assert!(row(&buf, 2).contains("sample-daemon"));
        assert!(row(&buf, 2).contains("arch: aarch64"));
    }

    #[test]
    fn placeholders_say_reading_while_a_worker_read_is_outstanding() {
        // With no instances and no read in flight the column is honest about
        // there being nothing live; with a read outstanding it says so instead of
        // sitting empty (the switcher's reads moved off the UI thread).
        let mut s = switcher();
        s.instances.clear();
        assert_eq!(
            s.empty_row(true, "(no live instances)"),
            "(no live instances)"
        );
        s.fetch = Some(super::Fetch::Sessions(std::sync::mpsc::channel().1));
        assert_eq!(s.empty_row(true, "(no live instances)"), "(reading bn…)");
        // A non-empty column narrowed to nothing by the filter is a no-match, not
        // a pending read.
        assert_eq!(s.empty_row(false, "(no live instances)"), "(no match)");
    }

    fn target(selector: &str) -> TargetItem {
        TargetItem {
            selector: selector.into(),
            active: true,
            analysis_state: "full".into(),
        }
    }

    #[test]
    fn a_filter_typed_before_sessions_land_snaps_the_instance_cursor() {
        // `/`-filtering is accepted while the initial session read is in flight,
        // and it narrows an empty list, so nothing snaps at type time. When the
        // list arrives the focused instance (`cur_instance`) may not match the
        // filter: leaving the cursor there shows no selection in the visible
        // rows and lets Enter switch to a hidden instance.
        let mut s = switcher();
        s.instances.clear();
        s.targets.clear();
        s.targets_of = String::new();
        s.preview_of = String::new();
        s.focus = Focus::Instances;
        s.filter = "inst-b".into();
        s.searching = true;
        // Cached so `pump` never spawns a `bn` read from the test.
        s.tcache.insert("inst-b".into(), vec![target("sample-updater")]);
        s.icache
            .insert("inst-b\0sample-updater".into(), vec!["arch: aarch64".into()]);

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(vec![
            Instance {
                instance_id: "inst-a".into(),
                binaries: Vec::new(),
                started_at: "2026-01-01T00:00:00Z".into(),
            },
            Instance {
                instance_id: "inst-b".into(),
                binaries: Vec::new(),
                started_at: "2026-01-01T00:00:00Z".into(),
            },
        ])
        .unwrap();
        s.fetch = Some(super::Fetch::Sessions(rx));
        s.poll();

        // cur_instance is inst-a (row 0), which the filter hides.
        assert_eq!(s.cur_inst_id().as_deref(), Some("inst-b"));
        assert!(s.visible_insts().contains(&s.inst_sel));
        // ...and the targets pumped are the visible instance's, not the hidden one's.
        assert_eq!(s.targets_of, "inst-b");
    }

    #[test]
    fn adopting_a_cached_target_list_lands_on_a_visible_row() {
        // Same hazard one column over: a filter typed while the target column was
        // empty leaves row 0 hidden, so the cursor must not default to it.
        let mut s = switcher();
        s.focus = Focus::Targets;
        s.filter = "updater".into();
        s.searching = true;
        s.targets_of = String::new();
        s.preview_of = String::new();
        s.tcache.insert(
            "inst-a".into(),
            vec![target("sample-daemon"), target("sample-updater")],
        );
        s.icache
            .insert("inst-a\0sample-updater".into(), vec!["arch: aarch64".into()]);

        s.pump();

        assert_eq!(s.tgt_sel, 1);
        assert!(s.visible_tgts().contains(&s.tgt_sel));
    }

    #[test]
    fn target_info_keeps_only_the_known_keys() {
        let raw = "  kind: elf\n  arch: aarch64\n  noise: ignored\n  functions: 412\n";
        assert_eq!(
            format_info(raw),
            vec!["kind: elf", "arch: aarch64", "functions: 412"]
        );
        assert_eq!(format_info("nothing useful"), vec!["(no info)"]);
    }
}
