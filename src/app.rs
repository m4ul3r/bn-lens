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
use crate::target_menu::{Outcome as TgtOutcome, TargetMenu};
use crate::types::TypesList;
use crate::ui;
use crate::viewer::{Exit, Viewer};
use crossterm::cursor::Show;
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

/// Minimum spacing between partner-status polls.
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// What a finished ctx rebuild does to the UI. A `^R` refresh re-reads the *same*
/// instance/target, so the picker's history and the open viewer survive it; a
/// re-point lands on a different binary, so every cached list and the open viewer
/// belong to the old target and have to go.
#[derive(Clone, Copy, PartialEq)]
enum Rebuild {
    Refresh,
    Repoint,
}

/// An in-flight ctx rebuild running on a worker thread, so the UI keeps drawing
/// (a counting banner) while the ~1s of sequential `bn` calls run off-thread.
struct Refreshing {
    started: Instant,
    kind: Rebuild,
    rx: Receiver<Result<Ctx, String>>,
}

/// An in-flight partner-status poll. Both `herdr` reads it needs are untimed
/// `Command::output()` calls, and running them inline at 1 Hz put two subprocess
/// spawns in front of every keystroke — a slow or hung `herdr` stopped redraws
/// and stopped accepting keys, `q` included. The answer is purely advisory (a
/// header glyph and the `◆ agent` group), so a late or dropped one costs nothing.
struct StatusPoll {
    rx: Receiver<PartnerStatus>,
}

struct PartnerStatus {
    partner: Option<crate::herdr::PaneAgent>,
    /// Raw pane scrollback; scanned for agent-mentioned symbols on delivery.
    transcript: String,
}

/// Whether a fresh partner poll may start. Pure so the scheduling rule is
/// testable: never two at once (a slow `herdr` must not queue up spawns), never
/// while a ctx rebuild owns the screen (it would stutter the banner's counter and
/// can't be acted on anyway), and at most one per [`POLL_INTERVAL`].
fn poll_due(since_last: Option<Duration>, in_flight: bool, rebuilding: bool) -> bool {
    !in_flight && !rebuilding && since_last.is_none_or(|d| d >= POLL_INTERVAL)
}

/// Runs its closure when dropped, however the scope is left: a normal return, an
/// error propagated by `?`, or a panic unwinding through it. The terminal
/// teardown hangs off this because every `?` in the event loop jumped over the
/// cleanup that used to sit at the bottom of it, leaving a raw-mode shell that
/// needed `reset`.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnDrop<F> {
    fn new(f: F) -> Self {
        OnDrop(Some(f))
    }
}

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// Put the terminal back the way we found it: cooked mode, primary screen, mouse
/// reporting off, cursor shown again (ratatui hides it while drawing and leaving
/// the alternate screen does not restore `?25h`). Every step is best-effort and
/// idempotent, so this is safe to call from `Drop` and from a panic hook.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        Show
    );
}

/// An in-flight `p` usage peek running on a worker thread. The requesting view
/// already shows its popup shell with a loading line; unlike a refresh, input
/// stays live (the popup owns keys, so Esc dismisses it — the late result is
/// then discarded on delivery rather than the `bn` calls being interrupted).
struct Peeking {
    started: Instant,
    /// The view whose popup requested the report (its keys stay captured while
    /// the popup is open, so it can't change out from under us unnoticed).
    view: AppView,
    /// Delivery is matched on the popup's address: a closed or re-targeted
    /// popup rejects the lines and the stale report is dropped.
    addr: String,
    rx: Receiver<Vec<String>>,
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
    /// Quick target dropdown (click the `-t` crumb) — same instance only.
    tgt_menu: Option<TargetMenu>,
    viewer: Option<Viewer>,
    switcher: Option<Switcher>,
    help: Help,
    area: Rect,
    bn_bin: String,
    agent_pane: String,
    herdr: String,
    partner: Option<crate::herdr::PaneAgent>,
    /// When the last partner poll was *started* (not delivered).
    last_poll: Option<Instant>,
    status_poll: Option<StatusPoll>,
    refreshing: Option<Refreshing>,
    peeking: Option<Peeking>,
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
            tgt_menu: None,
            viewer: None,
            switcher: None,
            help: Help::default(),
            area: Rect::new(0, 0, 0, 0),
            bn_bin,
            agent_pane,
            herdr,
            partner: None,
            last_poll: None,
            status_poll: None,
            refreshing: None,
            peeking: None,
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
        self.start_rebuild(Some(instance), tgt, Rebuild::Repoint);
    }

    /// Re-point the lens at another target of the *current* instance (from the
    /// `-t` crumb dropdown); keep current on failure.
    fn apply_target(&mut self, target: String) {
        if target == self.ctx.target {
            return; // already there — nothing to rebuild
        }
        let instance =
            (self.ctx.instance_label != "(default)").then(|| self.ctx.instance_label.clone());
        self.start_rebuild(instance, Some(target), Rebuild::Repoint);
    }

    /// Open the target dropdown anchored under the `-t` crumb. An empty list
    /// (no targets, or a failed `bn target list`) doesn't open — the failure
    /// surfaces through the shared backend-error bar instead.
    fn open_target_menu(&mut self, anchor_x: u16) {
        let items = self.ctx.bn.target_list();
        if items.is_empty() {
            return;
        }
        self.tgt_menu = Some(TargetMenu::new(items, &self.ctx.target, anchor_x));
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
            Action::PeekUsage { addr, hint } => {
                self.start_peek(addr, hint);
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
        let instance =
            (self.ctx.instance_label != "(default)").then(|| self.ctx.instance_label.clone());
        let target = (!self.ctx.target.is_empty()).then(|| self.ctx.target.clone());
        self.start_rebuild(instance, target, Rebuild::Refresh);
    }

    /// Build a ctx on a worker thread. Both the `^R` refresh and a switcher /
    /// `-t` re-point come through here: `Ctx::build` is ~1s of sequential `bn`
    /// calls, and running the re-point inline froze the TUI on a stale frame with
    /// no indication it was working (`^R` had already been moved off-thread).
    fn start_rebuild(&mut self, instance: Option<String>, target: Option<String>, kind: Rebuild) {
        if self.refreshing.is_some() {
            return;
        }
        let bn_bin = self.bn_bin.clone();
        let herdr = self.herdr.clone();
        let agent_pane = self.agent_pane.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Ctx::build(&bn_bin, &herdr, &agent_pane, instance, target));
        });
        self.refreshing = Some(Refreshing {
            started: Instant::now(),
            kind,
            rx,
        });
    }

    /// Adopt a rebuilt ctx that points somewhere new: every cached list and the
    /// open viewer describe the old target, so they go rather than being
    /// refreshed in place.
    fn adopt_repointed(&mut self, ctx: Ctx) {
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
    }

    /// Apply a finished refresh, keeping the picker's history and reloading the
    /// open viewer. A failed rebuild (or a dead worker) just drops the attempt
    /// and keeps the current ctx.
    fn poll_refresh(&mut self) {
        let Some(refreshing) = &self.refreshing else {
            return;
        };
        let kind = refreshing.kind;
        match refreshing.rx.try_recv() {
            Ok(Ok(ctx)) if kind == Rebuild::Repoint => {
                self.adopt_repointed(ctx);
                self.refresh_error = None;
                self.refreshing = None;
            }
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

    /// Kick off a usage report (`p` peek) on a worker thread, mirroring
    /// [`Self::start_refresh`]: for hot symbols the report's serial `bn` calls
    /// take seconds, and running them on the UI thread froze the TUI with no
    /// feedback. The requesting view has already opened its popup shell with a
    /// loading line; [`Self::poll_peek`] fills it in when the worker delivers.
    /// A new peek replaces any in-flight one (the old receiver is dropped, so
    /// the stale worker's send fails and its result vanishes).
    fn start_peek(&mut self, addr: String, hint: String) {
        let src = crate::usage::UsageSource::from_ctx(&self.ctx);
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let addr = addr.clone();
            std::thread::spawn(move || {
                let _ = tx.send(crate::usage::report(&src, &addr, &hint));
            });
        }
        self.peeking = Some(Peeking {
            started: Instant::now(),
            view: self.view,
            addr,
            rx,
        });
    }

    /// Deliver a finished peek to the popup that requested it, or — while it's
    /// still running — keep the popup's loading counter ticking. A popup that
    /// was closed (Esc) or re-targeted rejects delivery; the result is dropped
    /// and the peek stops being tracked.
    fn poll_peek(&mut self) {
        let Some(peeking) = &self.peeking else {
            return;
        };
        let (view, addr) = (peeking.view, peeking.addr.clone());
        match peeking.rx.try_recv() {
            Ok(lines) => {
                self.deliver_usage(view, &addr, lines);
                self.peeking = None;
            }
            Err(TryRecvError::Disconnected) => {
                self.deliver_usage(view, &addr, vec!["✗ usage worker died".into()]);
                self.peeking = None;
            }
            Err(TryRecvError::Empty) => {
                let elapsed = peeking.started.elapsed().as_secs_f32();
                if !self.deliver_usage(view, &addr, crate::usage::loading_lines(elapsed)) {
                    self.peeking = None;
                }
            }
        }
    }

    /// Route usage lines to `view`'s popup. False if the view (or its popup for
    /// `addr`) is gone — the caller drops the in-flight peek.
    fn deliver_usage(&mut self, view: AppView, addr: &str, lines: Vec<String>) -> bool {
        match view {
            AppView::Strings => self
                .strings
                .as_mut()
                .is_some_and(|s| s.set_usage_lines(addr, lines)),
            AppView::Imports => self
                .imports
                .as_mut()
                .is_some_and(|s| s.set_usage_lines(addr, lines)),
            AppView::Exports => self
                .exports
                .as_mut()
                .is_some_and(|s| s.set_usage_lines(addr, lines)),
            _ => false,
        }
    }

    /// While a refresh is in flight, a centered bottom banner shows elapsed time
    /// and that input is paused.
    fn draw_refresh(&self, buf: &mut Buffer, area: Rect) {
        let Some(refreshing) = &self.refreshing else {
            return;
        };
        let secs = refreshing.started.elapsed().as_secs_f32();
        let what = match refreshing.kind {
            Rebuild::Refresh => "refreshing bn context",
            Rebuild::Repoint => "switching bn target",
        };
        let label = format!("⟳ {what}…  {secs:.1}s   · Esc to cancel");
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

    /// Deliver a finished partner poll and start the next one when due (~1s).
    /// Both `herdr` reads run on a worker thread — see [`StatusPoll`] — so a slow
    /// or wedged `herdr` can no longer stall redraws or swallow keystrokes.
    fn poll_status(&mut self) {
        if let Some(poll) = &self.status_poll {
            match poll.rx.try_recv() {
                Ok(status) => {
                    self.partner = status.partner;
                    // Refresh the "recently viewed by agent" set from the pane's
                    // scrollback — but only trust it when the transcript actually
                    // concerns this target, so another binary's addresses/names
                    // don't get matched against the one in view (fail-closed
                    // provenance). The scan is pure and needs `&Ctx`, so it stays
                    // here; only the two subprocess spawns moved off-thread.
                    let tokens = self.ctx.recent_agent_tokens(&status.transcript);
                    self.picker.update_agent(tokens);
                    self.status_poll = None;
                }
                Err(TryRecvError::Disconnected) => self.status_poll = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.agent_pane.is_empty() {
            self.partner = None; // ask is off: nothing to poll, no herdr spawn
            return;
        }
        if !poll_due(
            self.last_poll.map(|t| t.elapsed()),
            self.status_poll.is_some(),
            self.refreshing.is_some(),
        ) {
            return;
        }
        self.last_poll = Some(Instant::now());
        let (tx, rx) = std::sync::mpsc::channel();
        let herdr = self.herdr.clone();
        let pane = self.agent_pane.clone();
        std::thread::spawn(move || {
            let partner = crate::herdr::pane_agent(&herdr, &pane);
            let transcript = crate::herdr::pane_read(&herdr, &pane, 400);
            let _ = tx.send(PartnerStatus {
                partner,
                transcript,
            });
        });
        self.status_poll = Some(StatusPoll { rx });
    }

    /// Deliver the switcher's worker reads (instance list, targets, target info)
    /// while the modal is open. A no-op otherwise.
    fn poll_switcher(&mut self) {
        if let Some(switcher) = &mut self.switcher {
            switcher.poll();
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
        // The target dropdown, like the view menu, owns every key while open.
        if let Some(tm) = &mut self.tgt_menu {
            match tm.on_key(k) {
                TgtOutcome::Continue => {}
                TgtOutcome::Cancel => self.tgt_menu = None,
                TgtOutcome::Apply(target) => {
                    self.tgt_menu = None;
                    self.apply_target(target);
                }
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
        if let Some(tm) = &mut self.tgt_menu {
            match tm.on_mouse(m) {
                TgtOutcome::Continue => {}
                TgtOutcome::Cancel => self.tgt_menu = None,
                TgtOutcome::Apply(target) => {
                    self.tgt_menu = None;
                    self.apply_target(target);
                }
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
        // Clicking the `-t <target>` crumb opens the quick target dropdown
        // (same guards as the title button).
        if self.viewer.is_none()
            && self.switcher.is_none()
            && !self.list_popup_open()
            && matches!(m.kind, crossterm::event::MouseEventKind::Down(_))
            && m.row == self.area.y
        {
            if let Some((x0, x1)) = ui::target_crumb_span(&self.ctx) {
                if m.column >= self.area.x + x0 && m.column < self.area.x + x1 {
                    self.open_target_menu(self.area.x + x0);
                    return false;
                }
            }
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
            // Cooked mode again even if `event::read` errors or panics.
            let _restore = OnDrop::new(|| {
                let _ = disable_raw_mode();
            });
            let _ = event::read();
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
    // A panic payload printed while the terminal is still raw + on the alternate
    // screen smears over the TUI and is unreadable, so restore first, then let
    // the normal hook print. The guard below covers the unwind (and every
    // `?`-return); this hook covers the message.
    let ui_thread = std::thread::current().id();
    let prior_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Only a panic on this thread ends the process. A worker's panic is
        // contained — its channel just disconnects and the app reports the
        // failure — so tearing the terminal down there would wreck a TUI that is
        // still running (`Ctx::build` joins its reads with `unwrap`, so that is a
        // reachable case).
        if std::thread::current().id() == ui_thread {
            restore_terminal();
        }
        prior_hook(info);
    }));

    enable_raw_mode()?;
    // Armed immediately: from here on, *every* exit path — normal, `?`, panic —
    // leaves a usable terminal. This is the crash class the Rust rewrite exists
    // to kill, and the teardown used to be reachable only from the loop's
    // `break Ok(())`.
    let _restore = OnDrop::new(restore_terminal);
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    let mut app = App::new(ctx);
    let res = loop {
        app.poll_refresh();
        app.poll_peek();
        app.poll_status();
        app.poll_switcher();
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
            if let Some(tm) = &mut app.tgt_menu {
                tm.render(app.area, f.buffer_mut());
            }
            app.help
                .render(app.area, f.buffer_mut(), app.help_context());
            app.draw_refresh(f.buffer_mut(), app.area);
        })?;

        // Tick faster while a refresh, peek or switcher read is in flight so
        // counters stay smooth and a delivered column appears promptly; otherwise
        // wake ~1/s to refresh the partner status.
        let busy = app.refreshing.is_some()
            || app.peeking.is_some()
            || app.switcher.as_ref().is_some_and(Switcher::pending);
        let timeout = if busy {
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

    res // `_restore` puts the terminal back as this scope ends
}

#[cfg(test)]
mod tests {
    use super::{poll_due, OnDrop, POLL_INTERVAL};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn the_terminal_guard_runs_on_return_and_while_unwinding() {
        static RESTORED: AtomicUsize = AtomicUsize::new(0);
        fn bump() {
            RESTORED.fetch_add(1, Ordering::SeqCst);
        }
        // Normal scope exit.
        {
            let _guard = OnDrop::new(bump);
        }
        assert_eq!(RESTORED.load(Ordering::SeqCst), 1);
        // The `?`-return path: an early return out of the guarded scope.
        fn bail() -> Result<(), &'static str> {
            let _guard = OnDrop::new(bump);
            Err("backend write failed")?;
            unreachable!()
        }
        assert!(bail().is_err());
        assert_eq!(RESTORED.load(Ordering::SeqCst), 2);
        // The panic path — the one that used to leave a raw-mode shell needing
        // `reset`, since the teardown sat after the event loop.
        let prior = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(|| {
            let _guard = OnDrop::new(bump);
            panic!("clamp min > max");
        });
        std::panic::set_hook(prior);
        assert!(result.is_err());
        assert_eq!(RESTORED.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn partner_polls_never_overlap_or_run_during_a_rebuild() {
        // First tick of the process: nothing polled yet, so it's due.
        assert!(poll_due(None, false, false));
        // Never a second spawn while one is outstanding — a hung `herdr` used to
        // block the loop; queueing more spawns behind it would be worse.
        assert!(!poll_due(None, true, false));
        // Never during a ctx rebuild: the banner owns the screen.
        assert!(!poll_due(None, false, true));
        // Throttled to POLL_INTERVAL.
        assert!(!poll_due(Some(Duration::from_millis(0)), false, false));
        assert!(!poll_due(
            Some(POLL_INTERVAL - Duration::from_millis(1)),
            false,
            false
        ));
        assert!(poll_due(Some(POLL_INTERVAL), false, false));
        assert!(poll_due(Some(Duration::from_secs(30)), false, false));
    }
}
