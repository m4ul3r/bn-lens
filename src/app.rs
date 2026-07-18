//! Terminal setup, the event loop, and the top-level Picker <-> Viewer state.
//! Also polls the launching agent's status so the pairing partner's state
//! ("working"/"done") is always visible on the header bar.

use crate::classes::ClassesList;
use crate::ctx::Ctx;
use crate::exports::ExportsList;
use crate::help::{Help, HelpContext};
use crate::imports::ImportsList;
use crate::marks::MarksList;
use crate::menu::{Choice, Menu};
use crate::picker::{Action, Picker};
use crate::strings::StringsList;
use crate::switch::{Outcome, Switcher};
use crate::types::TypesList;
use crate::ui;
use crate::viewer::{Exit, Viewer};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
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
    Imports,
    Exports,
    Classes,
    Types,
    Marks,
}

struct App {
    ctx: Ctx,
    view: AppView,
    picker: Picker,
    strings: Option<StringsList>, // built lazily on first switch to Strings
    imports: Option<ImportsList>, // built lazily on first switch to Imports
    exports: Option<ExportsList>, // built lazily on first switch to Exports
    classes: Option<ClassesList>, // built lazily on first switch to Classes
    types: Option<TypesList>,     // built lazily on first switch to Types
    marks: Option<MarksList>,     // built lazily on first switch to Marks
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
    /// Context-rebuild failures belong to the old ctx, so retain them here;
    /// cached-list/state failures are shared through `ctx.bn.last_error()`.
    /// Per-item viewer reads render their errors locally instead.
    refresh_error: Option<String>,
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
            imports: None,
            exports: None,
            classes: None,
            types: None,
            marks: None,
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
            refresh_error: None,
        }
    }

    fn open_switcher(&mut self) {
        self.switcher = Some(Switcher::new(&self.bn_bin, &self.ctx.instance_label));
    }

    /// Re-point the lens at (instance, target); keep current on failure.
    fn apply_switch(&mut self, instance: String, target: String) {
        let tgt = if target.is_empty() {
            None
        } else {
            Some(target)
        };
        match Ctx::build(
            &self.bn_bin,
            &self.herdr,
            &self.agent_pane,
            Some(instance),
            tgt,
        ) {
            Ok(ctx) => {
                self.ctx = ctx;
                self.picker = Picker::new(&self.ctx);
                self.strings = None;
                self.imports = None;
                self.exports = None;
                self.classes = None;
                self.types = None;
                self.marks = None;
                self.view = AppView::Symbols;
                self.viewer = None;
                self.refresh_error = None;
            }
            Err(error) => self.refresh_error = Some(error),
        }
    }

    /// The menu entry corresponding to the current top-level view.
    fn active_choice(&self) -> Choice {
        match self.view {
            AppView::Symbols => Choice::Symbols,
            AppView::Strings => Choice::Strings,
            AppView::Imports => Choice::Imports,
            AppView::Exports => Choice::Exports,
            AppView::Classes => Choice::Classes,
            AppView::Types => Choice::Types,
            AppView::Marks => Choice::Marks,
        }
    }

    /// Switch to a top-level list view, building its (lazy) list on first use.
    /// Marks rebuild every time — unlike the other lazy inventories,
    /// annotations change as you add them with `;`/`t`.
    fn set_view(&mut self, view: AppView) {
        match view {
            AppView::Symbols => {}
            AppView::Strings => {
                if self.strings.is_none() {
                    self.strings = Some(StringsList::new(&self.ctx));
                }
            }
            AppView::Imports => {
                if self.imports.is_none() {
                    self.imports = Some(ImportsList::new(&self.ctx));
                }
            }
            AppView::Exports => {
                if self.exports.is_none() {
                    self.exports = Some(ExportsList::new(&self.ctx));
                }
            }
            AppView::Classes => {
                if self.classes.is_none() {
                    self.classes = Some(ClassesList::new(&self.ctx));
                }
            }
            AppView::Types => {
                if self.types.is_none() {
                    self.types = Some(TypesList::new(&self.ctx));
                }
            }
            AppView::Marks => self.marks = Some(MarksList::new(&self.ctx)),
        }
        self.view = view;
        self.viewer = None;
    }

    /// `v` from a list: cycle through the top-level views in order (the keyboard
    /// twin of the `m` menu's view picker).
    fn cycle_app_view(&mut self, direction: i32) {
        const ORDER: &[AppView] = &[
            AppView::Symbols,
            AppView::Strings,
            AppView::Imports,
            AppView::Exports,
            AppView::Classes,
            AppView::Types,
            AppView::Marks,
        ];
        let current = ORDER.iter().position(|v| *v == self.view).unwrap_or(0) as i32;
        let next = ORDER[(current + direction).rem_euclid(ORDER.len() as i32) as usize];
        self.set_view(next);
    }

    /// Act on a menu selection. Returns true to quit the app.
    fn choose(&mut self, choice: Choice) -> bool {
        match choice {
            Choice::Symbols => self.set_view(AppView::Symbols),
            Choice::Strings => self.set_view(AppView::Strings),
            Choice::Imports => self.set_view(AppView::Imports),
            Choice::Exports => self.set_view(AppView::Exports),
            Choice::Classes => self.set_view(AppView::Classes),
            Choice::Types => self.set_view(AppView::Types),
            Choice::Marks => self.set_view(AppView::Marks),
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
            Action::Home => {
                self.set_view(AppView::Symbols);
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
                if let Some(imports) = &mut self.imports {
                    imports.refresh(&self.ctx);
                }
                if let Some(exports) = &mut self.exports {
                    exports.refresh(&self.ctx);
                }
                if let Some(classes) = &mut self.classes {
                    classes.refresh(&self.ctx);
                }
                if let Some(types) = &mut self.types {
                    types.refresh(&self.ctx);
                }
                if let Some(marks) = &mut self.marks {
                    marks.refresh(&self.ctx);
                }
                if let Some(viewer) = &mut self.viewer {
                    viewer.reload(&self.ctx);
                }
                self.refresh_error = None;
                self.refreshing = None;
            }
            Ok(Err(error)) => {
                self.refresh_error = Some(error);
                self.refreshing = None;
            }
            Err(TryRecvError::Disconnected) => {
                self.refresh_error = Some("bn refresh worker disconnected".into());
                self.refreshing = None;
            }
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
        let label = format!("⟳ refreshing bn context…  {secs:.1}s   · Esc to cancel");
        let width = area.width as usize;
        let y = area.y + area.height.saturating_sub(1);
        let style = Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        // Fill the whole bottom row, then center the label on it.
        crate::ui::put_str(buf, area.x, y, " ".repeat(width), width, style);
        let label_width = label.chars().count();
        let x = area.x + ((width.saturating_sub(label_width)) / 2) as u16;
        crate::ui::put_str(buf, x, y, label, width, style);
    }

    /// Refresh the pairing partner (throttled to ~1s).
    fn poll_status(&mut self) {
        // Don't do blocking herdr I/O while a refresh owns the screen — it would
        // stutter the banner's counter and can't be acted on anyway.
        if self.refreshing.is_some() {
            return;
        }
        let now = Instant::now();
        let due = self.last_poll.map_or(true, |t| {
            now.duration_since(t) >= Duration::from_millis(1000)
        });
        if due {
            self.partner = if self.agent_pane.is_empty() {
                None
            } else {
                crate::herdr::pane_agent(&self.herdr, &self.agent_pane)
            };
            // refresh the "recently viewed by agent" set from the pane's
            // scrollback — but only trust it when the transcript actually
            // concerns this target, so another binary's addresses/names don't
            // get matched against the one in view (fail-closed provenance).
            if !self.agent_pane.is_empty() {
                let text = crate::herdr::pane_read(&self.herdr, &self.agent_pane, 400);
                let tokens = self.ctx.recent_agent_tokens(&text);
                self.picker.update_agent(tokens);
            }
            self.last_poll = Some(now);
        }
    }

    /// Overlay ask routing on the right of the header. Healthy pairing is short
    /// (`ask · ◐ grok working`); failures stay loud with the pane id so a
    /// mis-wired launch is diagnosable.
    fn draw_partner(&self, buf: &mut Buffer, area: Rect) {
        let (color, label) = if self.agent_pane.is_empty() {
            (Color::Red, " ⚠ ask off ".to_string())
        } else {
            match &self.partner {
                None => (Color::Red, format!(" ⚠ no agent on {} ", self.agent_pane)),
                Some(a) if !crate::herdr::same_agent_session(&self.ctx.agent_session, a) => (
                    Color::Red,
                    " ⚠ launching agent changed · ask blocked ".to_string(),
                ),
                Some(a) => {
                    let (g, c) = match a.status.as_str() {
                        "working" => ("◐", Color::Yellow),
                        "blocked" => ("⚠", Color::Red),
                        "done" => ("✓", Color::Cyan),
                        "idle" => ("○", Color::Green),
                        _ => ("·", Color::Gray),
                    };
                    let name = if a.agent.is_empty() {
                        "agent"
                    } else {
                        a.agent.as_str()
                    };
                    (c, format!(" ask · {g} {name} {} ", a.status))
                }
            }
        };
        let lw = label.chars().count() as u16;
        if lw >= area.width {
            return;
        }
        crate::ui::put_str(
            buf,
            area.x + area.width - lw,
            area.y,
            label,
            lw as usize,
            Style::default()
                .fg(color)
                .bg(ui::BAR_BG)
                .add_modifier(Modifier::BOLD),
        );
    }

    /// A command/context failure must dominate cached content. Without this,
    /// dead sessions degrade into plausible `(no layout)`/`no references` rows.
    fn draw_backend_error(&self, buf: &mut Buffer, area: Rect) {
        if area.height < 2 {
            return;
        }
        let Some(error) = self
            .refresh_error
            .as_ref()
            .cloned()
            .or_else(|| self.ctx.bn.last_error())
        else {
            return;
        };
        let width = area.width as usize;
        let recovery = if self.viewer.is_some() {
            "^R retry · q then i switch"
        } else {
            "^R retry · i switch"
        };
        let label = format!(" ⚠ BN COMMAND FAILED · {error} · {recovery} ");
        crate::ui::put_str(
            buf,
            area.x,
            area.y + 1,
            format!("{label:<width$}"),
            width,
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        );
    }

    /// True while the focused list/viewer is capturing raw text input (a search
    /// filter, or an ask/comment/tag/rename field) — so the global `?`/`^R`/`m`
    /// shortcuts must not steal those characters.
    fn capturing_text(&self) -> bool {
        if let Some(sw) = &self.switcher {
            return sw.is_searching();
        }
        if let Some(viewer) = &self.viewer {
            return viewer.is_capturing_text();
        }
        match self.view {
            AppView::Symbols => self.picker.is_searching(),
            AppView::Strings => self.strings.as_ref().is_some_and(StringsList::is_searching),
            AppView::Imports => self.imports.as_ref().is_some_and(ImportsList::is_searching),
            AppView::Exports => self.exports.as_ref().is_some_and(ExportsList::is_searching),
            AppView::Classes => self.classes.as_ref().is_some_and(ClassesList::is_searching),
            AppView::Types => self.types.as_ref().is_some_and(TypesList::is_searching),
            AppView::Marks => self.marks.as_ref().is_some_and(MarksList::is_searching),
        }
    }

    /// True while the active list has a self-managed overlay open (sections /
    /// usage popup) — so `m` shouldn't stack the dropdown on top of it.
    fn list_popup_open(&self) -> bool {
        if self.viewer.is_some() {
            return false;
        }
        match self.view {
            AppView::Symbols => self.picker.popup_open(),
            AppView::Strings => self.strings.as_ref().is_some_and(StringsList::popup_open),
            AppView::Imports => self.imports.as_ref().is_some_and(ImportsList::popup_open),
            AppView::Exports => self.exports.as_ref().is_some_and(ExportsList::popup_open),
            AppView::Classes => self.classes.as_ref().is_some_and(ClassesList::popup_open),
            AppView::Types => self.types.as_ref().is_some_and(TypesList::popup_open),
            AppView::Marks => self.marks.as_ref().is_some_and(MarksList::popup_open),
        }
    }

    fn on_key(&mut self, k: crossterm::event::KeyEvent) -> bool {
        let ctrl = k
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        // A refresh blocks input by design, but keep an escape hatch (Esc /
        // Ctrl-C) so a hung `bn` can't wedge the TUI unrecoverably.
        if self.refreshing.is_some() {
            if k.code == crossterm::event::KeyCode::Esc
                || (ctrl && k.code == crossterm::event::KeyCode::Char('c'))
            {
                self.refreshing = None; // abandon; the worker's result is ignored
            }
            return false;
        }
        if self.help.is_open() {
            self.help.on_key(k);
            return false;
        }
        // The dropdown, when open, owns every key until a choice or a dismiss —
        // checked before the global shortcuts so `?`/`^R` don't stack over it.
        if self.menu.is_open() {
            if let Some(choice) = self.menu.on_key(k) {
                return self.choose(choice);
            }
            return false;
        }
        // Global shortcuts, suppressed while a text field is capturing input so
        // `?`/`m`/`^R` stay typeable inside search/comment/ask/rename fields.
        let capturing = self.capturing_text();
        if !capturing {
            if k.code == crossterm::event::KeyCode::Char('?') {
                self.help.open(self.help_context());
                return false;
            }
            if ctrl && k.code == crossterm::event::KeyCode::Char('r') && self.switcher.is_none() {
                self.start_refresh();
                return false;
            }
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
        // `m` opens the view menu from a list — never in the viewer, while typing,
        // or over a list sub-popup.
        if k.code == crossterm::event::KeyCode::Char('m')
            && self.viewer.is_none()
            && !capturing
            && !self.list_popup_open()
        {
            self.menu.open(self.active_choice());
            return false;
        }
        // `v` cycles the top-level list views (keyboard twin of the menu picker),
        // under the same guards as `m`. In the viewer, `v` falls through to the
        // viewer's own view-cycle instead.
        if k.code == crossterm::event::KeyCode::Char('v')
            && self.viewer.is_none()
            && !capturing
            && !self.list_popup_open()
        {
            self.cycle_app_view(1);
            return false;
        }
        match &mut self.viewer {
            Some(v) => {
                let exit = v.on_key(k, &self.ctx);
                match exit {
                    Exit::Back => self.viewer = None,
                    Exit::Stay => {}
                    Exit::Reload => self.start_refresh(),
                    // Comment/tag: reload just the viewer so the annotation
                    // renders, without the full-ctx worker refresh.
                    Exit::ReloadView => {
                        if let Some(v) = &mut self.viewer {
                            v.reload(&self.ctx);
                        }
                        // The annotation also changed the Marks inventory. It's
                        // cached and only rebuilt on an explicit view switch, so
                        // returning to it via q/Back would show stale data —
                        // refresh it now (annotations live in the bn instance, so
                        // no ctx rebuild is needed).
                        if let Some(marks) = &mut self.marks {
                            marks.refresh(&self.ctx);
                        }
                    }
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
                    AppView::Imports => self
                        .imports
                        .as_mut()
                        .map_or(Action::None, |s| s.on_key(k, &self.ctx)),
                    AppView::Exports => self
                        .exports
                        .as_mut()
                        .map_or(Action::None, |s| s.on_key(k, &self.ctx)),
                    AppView::Classes => self
                        .classes
                        .as_mut()
                        .map_or(Action::None, |s| s.on_key(k, &self.ctx)),
                    AppView::Types => self
                        .types
                        .as_mut()
                        .map_or(Action::None, |s| s.on_key(k, &self.ctx)),
                    AppView::Marks => self.marks.as_mut().map_or(Action::None, |s| s.on_key(k)),
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
        // Clicking the `bn lens` title toggles the dropdown (list mode only, and
        // not over a list sub-popup).
        if self.viewer.is_none()
            && self.switcher.is_none()
            && !self.list_popup_open()
            && Menu::hit_title(self.area, m)
        {
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
                AppView::Imports => {
                    if let Some(s) = &mut self.imports {
                        s.on_mouse(m, self.area);
                    }
                }
                AppView::Exports => {
                    if let Some(s) = &mut self.exports {
                        s.on_mouse(m, self.area);
                    }
                }
                AppView::Classes => {
                    if let Some(s) = &mut self.classes {
                        s.on_mouse(m, self.area);
                    }
                }
                AppView::Types => {
                    if let Some(s) = &mut self.types {
                        s.on_mouse(m, self.area);
                    }
                }
                AppView::Marks => {
                    if let Some(s) = &mut self.marks {
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
                AppView::Imports => HelpContext::Imports,
                AppView::Exports => HelpContext::Exports,
                AppView::Classes => HelpContext::Classes,
                AppView::Types => HelpContext::Types,
                AppView::Marks => HelpContext::Marks,
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
                    AppView::Imports => {
                        if let Some(s) = &mut app.imports {
                            s.render(app.area, f.buffer_mut(), &app.ctx);
                        }
                    }
                    AppView::Exports => {
                        if let Some(s) = &mut app.exports {
                            s.render(app.area, f.buffer_mut(), &app.ctx);
                        }
                    }
                    AppView::Classes => {
                        if let Some(s) = &mut app.classes {
                            s.render(app.area, f.buffer_mut(), &app.ctx);
                        }
                    }
                    AppView::Types => {
                        if let Some(s) = &mut app.types {
                            s.render(app.area, f.buffer_mut(), &app.ctx);
                        }
                    }
                    AppView::Marks => {
                        if let Some(s) = &mut app.marks {
                            s.render(app.area, f.buffer_mut(), &app.ctx);
                        }
                    }
                },
            }
            app.draw_partner(f.buffer_mut(), app.area);
            app.draw_backend_error(f.buffer_mut(), app.area);
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
