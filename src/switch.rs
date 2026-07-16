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
        };
        s.reload_targets();
        s
    }

    fn cur_inst_id(&self) -> Option<String> {
        self.instances.get(self.inst_sel).map(|i| i.instance_id.clone())
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
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Cancel,
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                Focus::Instances => {
                    if self.inst_sel + 1 < self.instances.len() {
                        self.inst_sel += 1;
                        self.reload_targets();
                    }
                }
                Focus::Targets => {
                    if self.tgt_sel + 1 < self.targets.len() {
                        self.tgt_sel += 1;
                        self.reload_preview();
                    }
                }
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                Focus::Instances => {
                    if self.inst_sel > 0 {
                        self.inst_sel -= 1;
                        self.reload_targets();
                    }
                }
                Focus::Targets => {
                    if self.tgt_sel > 0 {
                        self.tgt_sel -= 1;
                        self.reload_preview();
                    }
                }
            },
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                if !self.targets.is_empty() {
                    self.focus = Focus::Targets;
                }
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => {
                self.focus = Focus::Instances;
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

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        // clear the whole area
        for y in area.y..area.y + area.height {
            buf.set_stringn(area.x, y, " ".repeat(area.width as usize), area.width as usize, Style::default());
        }
        let w = area.width;
        ui::render_bar(
            buf,
            area.x,
            area.y,
            w as usize,
            &[ratatui::text::Span::styled(
                " switch bn — j/k move · h/l columns · Enter select · ? help · Esc cancel",
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
            buf.set_stringn(c2x - 1, y, "│", 1, Style::default().add_modifier(Modifier::DIM));
            buf.set_stringn(c3x - 1, y, "│", 1, Style::default().add_modifier(Modifier::DIM));
        }

        // column 1 — instances
        let inst_rows: Vec<(String, Style)> = self
            .instances
            .iter()
            .map(|i| {
                let s = if i.instance_id == self.cur_instance {
                    Style::default().fg(Color::Green)
                } else if i.binaries.is_empty() {
                    Style::default().add_modifier(Modifier::DIM)
                } else {
                    Style::default()
                };
                (i.instance_id.clone(), s)
            })
            .collect();
        render_col(buf, c1x, body_y, c1w, body_h, "INSTANCES", &inst_rows, self.inst_sel, self.focus == Focus::Instances);

        // column 2 — targets (placeholder row when the instance has none open)
        let (tgt_rows, tgt_sel): (Vec<(String, Style)>, usize) = if self.targets.is_empty() {
            (
                vec![("(no targets open)".to_string(), Style::default().add_modifier(Modifier::DIM))],
                usize::MAX, // never highlight the placeholder
            )
        } else {
            (
                self.targets
                    .iter()
                    .map(|t| {
                        (
                            format!("{}{}", if t.active { "● " } else { "  " }, short_target(&t.selector)),
                            Style::default(),
                        )
                    })
                    .collect(),
                self.tgt_sel,
            )
        };
        render_col(buf, c2x, body_y, c2w, body_h, "TARGETS", &tgt_rows, tgt_sel, self.focus == Focus::Targets);

        // column 3 — target info preview
        buf.set_stringn(c3x, body_y, " INFO", c3w as usize, Style::default().add_modifier(Modifier::DIM));
        if self.preview.is_empty() {
            buf.set_stringn(c3x + 1, body_y + 1, "(no target selected)", (c3w - 1) as usize, Style::default().add_modifier(Modifier::DIM));
        }
        for (i, line) in self.preview.iter().take(body_h.saturating_sub(1)).enumerate() {
            buf.set_stringn(c3x + 1, body_y + 1 + i as u16, line, (c3w - 1) as usize, Style::default().fg(Color::Cyan));
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
    buf.set_stringn(x, top_y, format!(" {header}"), w as usize, Style::default().add_modifier(Modifier::DIM));
    let view_h = h.saturating_sub(1);
    // guard sel that is out of range (usize::MAX = "highlight nothing") so the
    // scroll offset can't underflow and skip every row.
    let vtop = if sel < rows.len() && sel >= view_h { sel + 1 - view_h } else { 0 };
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
        buf.set_stringn(x, y, format!(" {}", trunc(label, (w - 1) as usize)), w as usize, style);
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

/// Shorten a bndb selector for the column: keep the name, abbreviate the hash.
fn short_target(sel: &str) -> String {
    let base = sel.strip_suffix(".bndb").unwrap_or(sel);
    if let Some(dot) = base.rfind('.') {
        let (name, hash) = base.split_at(dot);
        if hash.len() > 7 {
            return format!("{name}.{}…", &hash[1..5]);
        }
    }
    sel.to_string()
}

fn format_info(raw: &str) -> Vec<String> {
    let keys = ["kind:", "arch:", "platform:", "image base:", "entry:", "analysis:", "functions:", "file:"];
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
