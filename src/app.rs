//! Terminal setup, the event loop, and the top-level Picker <-> Viewer state.
//! Also polls the launching agent's status so the pairing partner's state
//! ("working"/"done") is always visible on the header bar.

use crate::ctx::Ctx;
use crate::help::{Help, HelpContext};
use crate::menu::{Choice, Menu};
use crate::picker::{Action, Picker};
use crate::strings::StringsList;
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
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// An in-flight ctx rebuild running on a worker thread, so the UI keeps drawing
/// (a counting banner) while the ~1s of sequential `bn` calls run off-thread.
struct Refreshing {
    started: Instant,
    rx: Receiver<Result<Ctx, String>>,
}

/// Which top-level list the picker pane is showing.
#[derive(Clone, Copy, PartialEq)]
enum AppView {
    Symbols,
    Strings,
}

struct App {
    ctx: Ctx,
    view: AppView,
    picker: Picker,
    strings: Option<StringsList>, // built lazily on first switch to Strings
    menu: Menu,
    viewer: Option<Viewer>,
    switcher: Option<Switcher>,
    help: Help,
    area: Rect,
    bn_bin: String,
    agent_pane: String,
    herdr: String,
    partner: Option<crate::herdr::PaneAgent>,
    last_poll: Option<Instant>,
    refreshing: Option<Refreshing>,
}

impl App {
    fn new(ctx: Ctx) -> Self {
        let picker = Picker::new(&ctx);
        let agent_pane = ctx.agent_pane.clone();
        let herdr = ctx.herdr.clone();
        let bn_bin = ctx.bn.bin.clone();
        App {
            ctx,
            view: AppView::Symbols,
            picker,
            strings: None,
            menu: Menu::default(),
            viewer: None,
            switcher: None,
            help: Help::default(),
            area: Rect::new(0, 0, 0, 0),
            bn_bin,
            agent_pane,
            herdr,
            partner: None,
            last_poll: None,
            refreshing: None,
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
            self.strings = None;
            self.view = AppView::Symbols;
            self.viewer = None;
        }
    }

    /// The menu entry corresponding to the current top-level view.
    fn active_choice(&self) -> Choice {
        match self.view {
            AppView::Symbols => Choice::Symbols,
            AppView::Strings => Choice::Strings,
        }
    }

    /// Act on a menu selection. Returns true to quit the app.
    fn choose(&mut self, choice: Choice) -> bool {
        match choice {
            Choice::Symbols => {
                self.view = AppView::Symbols;
                self.viewer = None;
            }
            Choice::Strings => {
                if self.strings.is_none() {
                    self.strings = Some(StringsList::new(&self.ctx));
                }
                self.view = AppView::Strings;
                self.viewer = None;
            }
            Choice::Refresh => self.start_refresh(),
            Choice::SwitchBn => self.open_switcher(),
            Choice::Help => self.help.open(self.help_context()),
            Choice::Quit => return true,
        }
        false
    }

    /// Open a list action (from either the symbols or strings list) in the viewer.
    fn open_action(&mut self, action: Action) -> bool {
        match action {
            Action::OpenDecompile(name) => {
                if self.view == AppView::Symbols {
                    self.picker.record_open(&name);
                }
                self.viewer = Some(Viewer::new(&self.ctx, name, true));
                false
            }
            Action::OpenXrefs(name) => {
                if self.view == AppView::Symbols {
                    self.picker.record_open(&name);
                }
                self.viewer = Some(Viewer::new(&self.ctx, name, false));
                false
            }
            Action::Switch => {
                self.open_switcher();
                false
            }
            Action::Quit => true,
            Action::None => false,
        }
    }

    /// Kick off a ctx rebuild against the *same* instance/target on a worker
    /// thread, so changes the agent (or a lens mutation) made to the live bn
    /// instance — renamed functions, new symbols — show up. The event loop shows
    /// a counting banner and ignores input until [`poll_refresh`] applies it.
    fn start_refresh(&mut self) {
        if self.refreshing.is_some() {
            return;
        }
        let bn_bin = self.bn_bin.clone();
        let herdr = self.herdr.clone();
        let agent_pane = self.agent_pane.clone();
        let instance =
            (self.ctx.instance_label != "(default)").then(|| self.ctx.instance_label.clone());
        let target = (!self.ctx.target.is_empty()).then(|| self.ctx.target.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Ctx::build(&bn_bin, &herdr, &agent_pane, instance, target));
        });
        self.refreshing = Some(Refreshing {
            started: Instant::now(),
            rx,
        });
    }

    /// Apply a finished refresh, keeping the picker's history and reloading the
    /// open viewer. A failed rebuild (or a dead worker) just drops the attempt
    /// and keeps the current ctx.
    fn poll_refresh(&mut self) {
        let Some(refreshing) = &self.refreshing else {
            return;
        };
        match refreshing.rx.try_recv() {
            Ok(Ok(ctx)) => {
                self.ctx = ctx;
                self.picker.refresh(&self.ctx);
                if let Some(strings) = &mut self.strings {
                    strings.refresh(&self.ctx);
                }
                if let Some(viewer) = &mut self.viewer {
                    viewer.reload(&self.ctx);
                }
                self.refreshing = None;
            }
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => self.refreshing = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// While a refresh is in flight, a centered bottom banner shows elapsed time
    /// and that input is paused.
    fn draw_refresh(&self, buf: &mut Buffer, area: Rect) {
        let Some(refreshing) = &self.refreshing else {
            return;
        };
        let secs = refreshing.started.elapsed().as_secs_f32();
        let label = format!("⟳ refreshing bn context…  {secs:.1}s   · input paused");
        let width = area.width as usize;
        let y = area.y + area.height.saturating_sub(1);
        let style = Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        // Fill the whole bottom row, then center the label on it.
        buf.set_stringn(area.x, y, " ".repeat(width), width, style);
        let label_width = label.chars().count();
        let x = area.x + ((width.saturating_sub(label_width)) / 2) as u16;
        buf.set_stringn(x, y, label, width, style);
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
    /// `◐ → wD:p1 working`, or an explicit warning when no agent will receive an ask.
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
        // A refresh is blocking-by-design: swallow input until it lands.
        if self.refreshing.is_some() {
            return false;
        }
        if self.help.is_open() {
            self.help.on_key(k);
            return false;
        }
        let composing_question = self
            .viewer
            .as_ref()
            .is_some_and(Viewer::is_composing_question);
        if k.code == crossterm::event::KeyCode::Char('?') && !composing_question {
            self.help.open(self.help_context());
            return false;
        }
        // Ctrl-R: re-sync with the live bn instance (agent edits, renames).
        if k.code == crossterm::event::KeyCode::Char('r')
            && k.modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            && self.switcher.is_none()
            && !composing_question
        {
            self.start_refresh();
            return false;
        }
        // The dropdown, when open, captures keys until a choice or dismiss.
        if self.menu.is_open() {
            if let Some(choice) = self.menu.on_key(k) {
                return self.choose(choice);
            }
            return false;
        }
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
        // `m` opens the view menu from either list (not while in the viewer).
        if k.code == crossterm::event::KeyCode::Char('m') && self.viewer.is_none() {
            self.menu.open(self.active_choice());
            return false;
        }
        match &mut self.viewer {
            Some(v) => {
                match v.on_key(k, &self.ctx) {
                    Exit::Back => self.viewer = None,
                    Exit::Stay => {}
                    Exit::Reload => self.start_refresh(),
                }
                false
            }
            None => {
                let action = match self.view {
                    AppView::Symbols => self.picker.on_key(k),
                    AppView::Strings => self
                        .strings
                        .as_mut()
                        .map_or(Action::None, |s| s.on_key(k, &self.ctx)),
                };
                self.open_action(action)
            }
        }
    }

    /// Returns true to quit (a menu click can pick Quit).
    fn on_mouse(&mut self, m: crossterm::event::MouseEvent) -> bool {
        if self.refreshing.is_some() {
            return false;
        }
        if self.help.is_open() {
            self.help.on_mouse(m);
            return false;
        }
        if self.menu.is_open() {
            if let Some(choice) = self.menu.on_mouse(m) {
                return self.choose(choice);
            }
            return false;
        }
        // Clicking the `bn lens` title toggles the dropdown (list mode only).
        if self.viewer.is_none() && self.switcher.is_none() && Menu::hit_title(self.area, m) {
            self.menu.toggle(self.active_choice());
            return false;
        }
        if self.switcher.is_some() {
            return false;
        }
        match &mut self.viewer {
            Some(v) => v.on_mouse(m, &self.ctx),
            None => match self.view {
                AppView::Symbols => self.picker.on_mouse(m, self.area),
                AppView::Strings => {
                    if let Some(s) = &mut self.strings {
                        s.on_mouse(m, self.area);
                    }
                }
            },
        }
        false
    }

    fn help_context(&self) -> HelpContext {
        if self.switcher.is_some() {
            HelpContext::Switcher
        } else if let Some(viewer) = &self.viewer {
            if viewer.is_inspecting_stack() {
                HelpContext::Stack
            } else {
                HelpContext::Viewer
            }
        } else {
            match self.view {
                AppView::Symbols => HelpContext::Picker,
                AppView::Strings => HelpContext::Strings,
            }
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
        app.poll_refresh();
        app.poll_status();
        term.draw(|f| {
            app.area = f.area();
            match &mut app.viewer {
                Some(v) => v.render(app.area, f.buffer_mut(), &app.ctx),
                None => match app.view {
                    AppView::Symbols => app.picker.render(app.area, f.buffer_mut(), &app.ctx),
                    AppView::Strings => {
                        if let Some(s) = &mut app.strings {
                            s.render(app.area, f.buffer_mut(), &app.ctx);
                        }
                    }
                },
            }
            app.draw_partner(f.buffer_mut(), app.area);
            if let Some(sw) = &app.switcher {
                sw.render(app.area, f.buffer_mut());
            }
            let choice = app.active_choice();
            app.menu.render(app.area, f.buffer_mut(), choice);
            app.help
                .render(app.area, f.buffer_mut(), app.help_context());
            app.draw_refresh(f.buffer_mut(), app.area);
        })?;

        // Tick faster while refreshing so the banner's counter stays smooth;
        // otherwise wake ~1/s to refresh the partner status.
        let timeout = if app.refreshing.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(400)
        };
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if app.on_key(k) {
                        break Ok(());
                    }
                }
                Event::Mouse(m) => {
                    if app.on_mouse(m) {
                        break Ok(());
                    }
                }
                _ => {}
            }
        }
    };

    disable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, LeaveAlternateScreen, DisableMouseCapture)?;
    res
}
