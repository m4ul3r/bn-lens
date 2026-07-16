//! Terminal setup, the event loop, and the top-level Picker <-> Viewer state.
//! Also polls the launching agent's status so the pairing partner's state
//! ("working"/"done") is always visible on the header bar.

use crate::ctx::Ctx;
use crate::picker::{Action, Picker};
use crate::switch::{Outcome, Switcher};
use crate::ui;
use crate::viewer::{Exit, Viewer};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Terminal;
use std::io;
use std::time::{Duration, Instant};

struct App {
    ctx: Ctx,
    picker: Picker,
    viewer: Option<Viewer>,
    switcher: Option<Switcher>,
    area: Rect,
    bn_bin: String,
    agent_pane: String,
    herdr: String,
    partner: Option<crate::herdr::PaneAgent>,
    last_poll: Option<Instant>,
}

impl App {
    fn new(ctx: Ctx) -> Self {
        let picker = Picker::new(&ctx);
        let agent_pane = ctx.agent_pane.clone();
        let herdr = ctx.herdr.clone();
        let bn_bin = ctx.bn.bin.clone();
        App {
            ctx,
            picker,
            viewer: None,
            switcher: None,
            area: Rect::new(0, 0, 0, 0),
            bn_bin,
            agent_pane,
            herdr,
            partner: None,
            last_poll: None,
        }
    }

    fn open_switcher(&mut self) {
        self.switcher = Some(Switcher::new(&self.bn_bin, &self.ctx.instance_label));
    }

    /// Re-point the lens at (instance, target); keep current on failure.
    fn apply_switch(&mut self, instance: String, target: String) {
        let tgt = if target.is_empty() { None } else { Some(target) };
        if let Ok(ctx) =
            Ctx::build(&self.bn_bin, &self.herdr, &self.agent_pane, Some(instance), tgt)
        {
            self.ctx = ctx;
            self.picker = Picker::new(&self.ctx);
            self.viewer = None;
        }
    }

    /// Refresh the pairing partner (throttled to ~1s).
    fn poll_status(&mut self) {
        let now = Instant::now();
        let due = self
            .last_poll
            .map_or(true, |t| now.duration_since(t) >= Duration::from_millis(1000));
        if due {
            self.partner = if self.agent_pane.is_empty() {
                None
            } else {
                crate::herdr::pane_agent(&self.herdr, &self.agent_pane)
            };
            // refresh the "recently viewed by agent" set from the pane's scrollback
            if !self.agent_pane.is_empty() {
                let text = crate::herdr::pane_read(&self.herdr, &self.agent_pane, 400);
                self.picker.update_agent(crate::ctx::scan_recent(&text));
            }
            self.last_poll = Some(now);
        }
    }

    /// Overlay the ask destination + partner status on the right of the header:
    /// `◐ → wD:p1 working`, or an explicit warning when no agent will receive `?`.
    fn draw_partner(&self, buf: &mut Buffer, area: Rect) {
        let (color, label) = if self.agent_pane.is_empty() {
            (Color::Red, " ⚠ ask off: no launching pane ".to_string())
        } else {
            match &self.partner {
                None => (Color::Red, format!(" ⚠ {} has no agent ", self.agent_pane)),
                Some(a) => {
                    let (g, c) = match a.status.as_str() {
                        "working" => ("◐", Color::Yellow),
                        "blocked" => ("⚠", Color::Red),
                        "done" => ("✓", Color::Cyan),
                        "idle" => ("○", Color::Green),
                        _ => ("·", Color::Gray),
                    };
                    (c, format!(" {g} → {} {} {} ", self.agent_pane, a.agent, a.status))
                }
            }
        };
        let lw = label.chars().count() as u16;
        if lw >= area.width {
            return;
        }
        buf.set_stringn(
            area.x + area.width - lw,
            area.y,
            label,
            lw as usize,
            Style::default().fg(color).bg(ui::BAR_BG).add_modifier(Modifier::BOLD),
        );
    }

    fn on_key(&mut self, k: crossterm::event::KeyEvent) -> bool {
        if let Some(sw) = &mut self.switcher {
            match sw.on_key(k) {
                Outcome::Continue => {}
                Outcome::Cancel => self.switcher = None,
                Outcome::Apply(inst, tgt) => {
                    self.apply_switch(inst, tgt);
                    self.switcher = None;
                }
            }
            return false;
        }
        match &mut self.viewer {
            Some(v) => {
                match v.on_key(k, &self.ctx) {
                    Exit::Back => self.viewer = None,
                    Exit::Stay => {}
                }
                false
            }
            None => match self.picker.on_key(k) {
                Action::OpenDecompile(n) => {
                    self.picker.record_open(&n);
                    self.viewer = Some(Viewer::new(&self.ctx, n, true));
                    false
                }
                Action::OpenXrefs(n) => {
                    self.picker.record_open(&n);
                    self.viewer = Some(Viewer::new(&self.ctx, n, false));
                    false
                }
                Action::Switch => {
                    self.open_switcher();
                    false
                }
                Action::Quit => true,
                Action::None => false,
            },
        }
    }

    fn on_mouse(&mut self, m: crossterm::event::MouseEvent) {
        if self.switcher.is_some() {
            return;
        }
        match &mut self.viewer {
            Some(v) => v.on_mouse(m, &self.ctx),
            None => self.picker.on_mouse(m, self.area),
        }
    }
}

pub fn run() -> i32 {
    let ctx = match Ctx::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\n  bn lens: {e}\n\n  [enter to close]");
            let _ = enable_raw_mode();
            let _ = event::read();
            let _ = disable_raw_mode();
            return 1;
        }
    };
    if let Err(e) = event_loop(ctx) {
        eprintln!("bn lens: {e}");
        return 1;
    }
    0
}

fn event_loop(ctx: Ctx) -> io::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    let mut app = App::new(ctx);
    let res = loop {
        app.poll_status();
        term.draw(|f| {
            app.area = f.area();
            match &mut app.viewer {
                Some(v) => v.render(app.area, f.buffer_mut(), &app.ctx),
                None => app.picker.render(app.area, f.buffer_mut(), &app.ctx),
            }
            app.draw_partner(f.buffer_mut(), app.area);
            if let Some(sw) = &app.switcher {
                sw.render(app.area, f.buffer_mut());
            }
        })?;

        // Wake periodically even without input, so the partner status refreshes.
        if event::poll(Duration::from_millis(400))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if app.on_key(k) {
                        break Ok(());
                    }
                }
                Event::Mouse(m) => app.on_mouse(m),
                _ => {}
            }
        }
    };

    disable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, LeaveAlternateScreen, DisableMouseCapture)?;
    res
}
