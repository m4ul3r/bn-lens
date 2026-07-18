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
    /// Type-ahead filter over the *focused* column (instances or targets).
    filter: String,
    searching: bool,
}

impl Switcher {
    pub fn new(bn_bin: &str, cur_instance: &str) -> Self {
        let instances = Bn::session_list(bn_bin);
        let inst_sel = instances
            .iter()
            .position(|i| i.instance_id == cur_instance)
            .unwrap_or(0);
        let mut s = Switcher {
            bn_bin: bn_bin.to_string(),
            cur_instance: cur_instance.to_string(),
            instances,
            inst_sel,
            targets: Vec::new(),
            tgt_sel: 0,
            focus: Focus::Instances,
            preview: Vec::new(),
            tcache: std::collections::HashMap::new(),
            icache: std::collections::HashMap::new(),
            filter: String::new(),
            searching: false,
        };
        s.reload_targets();
        s
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
            self.reload_targets();
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
            self.reload_preview();
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
                        self.reload_targets();
                    }
                }
            }
            Focus::Targets => {
                let vis = self.visible_tgts();
                if !vis.contains(&self.tgt_sel) {
                    if let Some(&first) = vis.first() {
                        self.tgt_sel = first;
                        self.reload_preview();
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

    fn reload_targets(&mut self) {
        self.tgt_sel = 0;
        self.targets = match self.cur_inst_id() {
            Some(id) => self
                .tcache
                .entry(id.clone())
                .or_insert_with(|| Bn::new(self.bn_bin.clone(), Some(id), None).target_list())
                .clone(),
            None => Vec::new(),
        };
        self.reload_preview();
    }

    fn reload_preview(&mut self) {
        self.preview = match (self.cur_inst_id(), self.targets.get(self.tgt_sel)) {
            (Some(id), Some(t)) => {
                let key = format!("{id}\0{}", t.selector);
                let bn_bin = self.bn_bin.clone();
                let sel = t.selector.clone();
                self.icache
                    .entry(key)
                    .or_insert_with(|| {
                        let raw = Bn::new(bn_bin, Some(id), Some(sel)).target_info_raw();
                        format_info(&raw)
                    })
                    .clone()
            }
            _ => Vec::new(),
        };
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

    fn move_focused(&mut self, delta: i64) {
        match self.focus {
            Focus::Instances => self.move_inst(delta),
            Focus::Targets => self.move_tgt(delta),
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
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
        let header = if self.searching {
            format!(" switch bn — /{}   (Enter keep · Esc clear)", self.filter)
        } else if !self.filter.is_empty() {
            format!(
                " switch bn — filter: {}   j/k move · h/l cols · / filter · Enter select · Esc clear",
                self.filter
            )
        } else {
            " switch bn — j/k move · h/l cols · / filter · Enter select · ? help · Esc cancel"
                .to_string()
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
        let c2w = (rem * 6 / 10).min(46);
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
                "(no match)".to_string(),
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
                    "(no targets open)".to_string(),
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

        // column 3 — target info preview
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
                "(no target selected)",
                (c3w - 1) as usize,
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
                (c3w - 1) as usize,
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
            format!(" {}", trunc(label, (w - 1) as usize)),
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
