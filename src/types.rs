//! The types view: every type in the binary's type system, with a layout peek
//! (`Enter`/`p` → `bn types show`) and an in-TUI editor (`n`) for authoring a
//! new type — write a C declaration across multiple lines, validate it with a
//! live parse-check (`^P`, via `types declare --preview`), and commit it into
//! the live bn instance (`^S`). Like every other write here it lands in the
//! live database immediately but persists to the on-disk `.bndb` only on an
//! explicit `bn save`.

use crate::bn::TypeCheck;
use crate::ctx::Ctx;
use crate::picker::Action;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

struct TyItem {
    name: String,
    kind: String,
}

enum Mode {
    Normal,
    Search,
}

/// A scrollable layout peek (`bn types show`).
struct Show {
    title: String,
    lines: Vec<String>,
    off: usize,
}

/// A multi-line C-declaration editor for authoring a new type.
struct TypeEditor {
    lines: Vec<String>,
    row: usize,
    col: usize,
    /// Vertical scroll offset (first visible editor line).
    top: usize,
    /// Validation / error feedback under the text area.
    status: String,
    /// True once a `^P` check or `^S` commit reported the text parses.
    ok: bool,
}

/// What an editor keypress resolved to.
enum EditorResult {
    None,
    Cancel,
    Committed(String),
}

pub struct TypesList {
    items: Vec<TyItem>,
    kwidth: usize,
    filter: String,
    prev_filter: String,
    mode: Mode,
    sel: usize,
    top: usize,
    pending_g: bool,
    show: Option<Show>,
    editor: Option<TypeEditor>,
}

/// Composite/aggregate types lead (the interesting ones for RE); primitives and
/// refs trail. Within a rank, by name.
fn rank(kind: &str) -> u8 {
    match kind {
        "struct" | "union" => 0,
        "enum" => 1,
        "function" => 2,
        _ => 3,
    }
}

fn kind_color(kind: &str) -> Color {
    match kind {
        "struct" | "union" => Color::Cyan,
        "enum" => Color::Yellow,
        "function" => Color::Blue,
        _ => crate::theme::NAME,
    }
}

impl TypeEditor {
    fn new() -> Self {
        TypeEditor {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            top: 0,
            status: "write a C declaration, e.g.  struct foo { uint32_t id; char name[16]; };".into(),
            ok: false,
        }
    }

    fn line_chars(&self, row: usize) -> Vec<char> {
        self.lines[row].chars().collect()
    }

    fn cur_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    fn insert_char(&mut self, ch: char) {
        let mut chars = self.line_chars(self.row);
        self.col = self.col.min(chars.len());
        chars.insert(self.col, ch);
        self.lines[self.row] = chars.into_iter().collect();
        self.col += 1;
        self.ok = false;
    }

    /// Split the current line at the cursor, carrying its leading indent to the
    /// new line (so struct bodies stay aligned).
    fn newline(&mut self) {
        let chars = self.line_chars(self.row);
        self.col = self.col.min(chars.len());
        let head: String = chars[..self.col].iter().collect();
        let tail: String = chars[self.col..].iter().collect();
        let indent: String = head.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        self.lines[self.row] = head;
        self.lines.insert(self.row + 1, format!("{indent}{tail}"));
        self.row += 1;
        self.col = indent.chars().count();
        self.ok = false;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let mut chars = self.line_chars(self.row);
            chars.remove(self.col - 1);
            self.lines[self.row] = chars.into_iter().collect();
            self.col -= 1;
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.cur_len();
            self.lines[self.row].push_str(&cur);
        }
        self.ok = false;
    }

    fn move_v(&mut self, delta: i64) {
        let target = (self.row as i64 + delta).clamp(0, self.lines.len() as i64 - 1) as usize;
        self.row = target;
        self.col = self.col.min(self.cur_len());
    }

    fn move_h(&mut self, delta: i64) {
        let target = self.col as i64 + delta;
        if target < 0 {
            if self.row > 0 {
                self.row -= 1;
                self.col = self.cur_len();
            }
        } else if target as usize > self.cur_len() {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.col = 0;
            }
        } else {
            self.col = target as usize;
        }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }

    /// `^P`: validate without committing.
    fn check(&mut self, ctx: &Ctx) {
        if self.is_empty() {
            self.status = "nothing to check yet".into();
            self.ok = false;
            return;
        }
        match ctx.bn.type_declare_check(&self.text()) {
            TypeCheck::Ok(layouts) => {
                self.ok = true;
                let summary: Vec<String> = layouts
                    .iter()
                    .map(|l| {
                        let size = l
                            .layout
                            .lines()
                            .next()
                            .and_then(|line| line.split("// size=").nth(1))
                            .map(|s| format!("  size={}", s.trim()))
                            .unwrap_or_default();
                        format!("{}{size}", l.name)
                    })
                    .collect();
                self.status = format!("✓ parses · {}", summary.join(" · "));
            }
            TypeCheck::Err(err) => {
                self.ok = false;
                self.status = format!("✗ {err}");
            }
        }
    }

    /// `^S`: commit. Returns the type on success (caller closes + refreshes);
    /// keeps the editor open on a parse error.
    fn commit(&mut self, ctx: &Ctx) -> EditorResult {
        if self.is_empty() {
            self.status = "nothing to declare yet".into();
            return EditorResult::None;
        }
        match ctx.bn.type_declare(&self.text()) {
            Ok(count) => EditorResult::Committed(format!(
                "✓ declared {count} type(s)   (live · `bn save` to persist)"
            )),
            Err(err) => {
                self.ok = false;
                self.status = format!("✗ {err}");
                EditorResult::None
            }
        }
    }

    fn on_key(&mut self, k: KeyEvent, ctx: &Ctx) -> EditorResult {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return EditorResult::Cancel,
            KeyCode::Char('s') if ctrl => return self.commit(ctx),
            KeyCode::Char('p') if ctrl => self.check(ctx),
            KeyCode::Enter => self.newline(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Tab => {
                for _ in 0..4 {
                    self.insert_char(' ');
                }
            }
            KeyCode::Left => self.move_h(-1),
            KeyCode::Right => self.move_h(1),
            KeyCode::Up => self.move_v(-1),
            KeyCode::Down => self.move_v(1),
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.cur_len(),
            KeyCode::Char(ch) => self.insert_char(ch),
            _ => {}
        }
        EditorResult::None
    }
}

impl TypesList {
    pub fn new(ctx: &Ctx) -> Self {
        let items = Self::build(ctx);
        let kwidth = items
            .iter()
            .map(|it| it.kind.chars().count())
            .max()
            .unwrap_or(6)
            .clamp(6, 16);
        TypesList {
            items,
            kwidth,
            filter: String::new(),
            prev_filter: String::new(),
            mode: Mode::Normal,
            sel: 0,
            top: 0,
            pending_g: false,
            show: None,
            editor: None,
        }
    }

    pub fn refresh(&mut self, ctx: &Ctx) {
        self.items = Self::build(ctx);
        self.kwidth = self
            .items
            .iter()
            .map(|it| it.kind.chars().count())
            .max()
            .unwrap_or(6)
            .clamp(6, 16);
        self.sel = self.sel.min(self.filtered().len().saturating_sub(1));
        self.top = self.top.min(self.sel);
    }

    fn build(ctx: &Ctx) -> Vec<TyItem> {
        let mut items: Vec<TyItem> = ctx
            .bn
            .types_list()
            .into_iter()
            .map(|t| TyItem {
                name: t.name,
                kind: t.kind,
            })
            .collect();
        items.sort_by(|a, b| {
            rank(&a.kind)
                .cmp(&rank(&b.kind))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        items
    }

    pub fn is_searching(&self) -> bool {
        matches!(self.mode, Mode::Search) || self.editor.is_some()
    }

    pub fn popup_open(&self) -> bool {
        self.show.is_some() || self.editor.is_some()
    }

    fn struct_count(&self) -> usize {
        self.items
            .iter()
            .filter(|it| matches!(it.kind.as_str(), "struct" | "union"))
            .count()
    }

    fn filtered(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                f.is_empty()
                    || it.name.to_lowercase().contains(&f)
                    || it.kind.to_lowercase().contains(&f)
            })
            .collect()
    }

    fn move_sel(&mut self, delta: i64) {
        let len = self.filtered().len() as i64;
        if len == 0 {
            return;
        }
        self.sel = (self.sel as i64 + delta).clamp(0, len - 1) as usize;
    }

    fn current(&self) -> Option<&TyItem> {
        let rows = self.filtered();
        rows.get(self.sel).map(|&i| &self.items[i])
    }

    fn open_show(&mut self, ctx: &Ctx) {
        let Some(item) = self.current() else { return };
        let name = item.name.clone();
        self.show = Some(Show {
            title: format!("type {name}"),
            lines: ctx.bn.type_show(&name),
            off: 0,
        });
    }

    fn show_key(&mut self, k: KeyEvent) {
        let Some(show) = &mut self.show else { return };
        let n = show.lines.len();
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => self.show = None,
            KeyCode::Char('j') | KeyCode::Down => show.off = (show.off + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => show.off = show.off.saturating_sub(1),
            KeyCode::PageDown => show.off = (show.off + 10).min(n.saturating_sub(1)),
            KeyCode::PageUp => show.off = show.off.saturating_sub(10),
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: KeyEvent, ctx: &Ctx) -> Action {
        if self.editor.is_some() {
            let result = self
                .editor
                .as_mut()
                .map(|e| e.on_key(k, ctx))
                .unwrap_or(EditorResult::None);
            match result {
                EditorResult::None => {}
                EditorResult::Cancel => self.editor = None,
                EditorResult::Committed(_msg) => {
                    self.editor = None;
                    self.items = Self::build(ctx);
                    self.sel = 0;
                    self.top = 0;
                }
            }
            return Action::None;
        }
        if self.show.is_some() {
            self.show_key(k);
            return Action::None;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Mode::Search = self.mode {
            match k.code {
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    self.open_show(ctx);
                }
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

        if self.pending_g {
            self.pending_g = false;
            if k.code == KeyCode::Char('g') {
                self.sel = 0;
                return Action::None;
            }
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
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
            KeyCode::Char('n') => self.editor = Some(TypeEditor::new()),
            KeyCode::Enter | KeyCode::Char('p') => self.open_show(ctx),
            _ => {}
        }
        Action::None
    }

    pub fn on_mouse(&mut self, m: MouseEvent, area: Rect) {
        if let Some(show) = &mut self.show {
            let n = show.lines.len();
            match m.kind {
                MouseEventKind::ScrollUp => show.off = show.off.saturating_sub(3),
                MouseEventKind::ScrollDown => show.off = (show.off + 3).min(n.saturating_sub(1)),
                MouseEventKind::Down(_) => self.show = None,
                _ => {}
            }
            return;
        }
        if self.editor.is_some() {
            return; // the editor is keyboard-driven
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.move_sel(-3),
            MouseEventKind::ScrollDown => self.move_sel(3),
            MouseEventKind::Down(_) => {
                let row = m.row.saturating_sub(area.y + 2) as usize;
                let idx = self.top + row;
                if idx < self.filtered().len() {
                    self.sel = idx;
                }
            }
            _ => {}
        }
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &Ctx) {
        let rows = self.filtered();
        let listh = area.height.saturating_sub(3) as usize;
        if self.sel < self.top {
            self.top = self.sel;
        }
        if listh > 0 && self.sel >= self.top + listh {
            self.top = self.sel + 1 - listh;
        }

        let x0 = area.x;
        let w = area.width as usize;
        let mut bar = crate::ui::crumbs(ctx);
        bar.push(Span::styled(
            format!(
                "   types  {}/{}  · {} struct/union",
                rows.len(),
                self.items.len(),
                self.struct_count()
            ),
            Style::default().add_modifier(Modifier::DIM),
        ));
        crate::ui::render_bar(buf, x0, area.y, w, &bar);

        let (state, keys) = match self.mode {
            Mode::Search => (
                format!(" /{}", self.filter),
                " type · ↑↓ pick · Enter show · Tab list · Esc cancel · ? help",
            ),
            Mode::Normal => (
                if self.filter.is_empty() {
                    " types · type system".to_string()
                } else {
                    format!(" types · filter: {}", self.filter)
                },
                " j/k move · / search · Enter/p show layout · n new type · m menu · q quit",
            ),
        };
        crate::ui::put_str(
            buf,
            x0,
            area.y + 1,
            state,
            w,
            Style::default().add_modifier(Modifier::DIM),
        );
        crate::ui::render_bar(
            buf,
            x0,
            area.y + area.height.saturating_sub(1),
            w,
            &[Span::styled(
                keys,
                Style::default().add_modifier(Modifier::DIM),
            )],
        );

        if rows.is_empty() {
            crate::ui::put_str(
                buf,
                x0 + 2,
                area.y + 3,
                "no types — press n to declare one",
                w.saturating_sub(4),
                Style::default().add_modifier(Modifier::DIM),
            );
        }

        for (row, &i) in rows.iter().enumerate().skip(self.top).take(listh) {
            let y = area.y + 2 + (row - self.top) as u16;
            let it = &self.items[i];
            let is_sel = row == self.sel;
            if is_sel {
                let text = format!("  {:<kw$}  {}", it.kind, it.name, kw = self.kwidth);
                crate::ui::put_str(
                    buf,
                    x0,
                    y,
                    format!("{text:<w$}"),
                    w,
                    Style::default().add_modifier(Modifier::REVERSED),
                );
                continue;
            }
            let spans = vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("{:<kw$}", it.kind, kw = self.kwidth),
                    Style::default()
                        .fg(kind_color(&it.kind))
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(format!("  {}", it.name), Style::default().fg(kind_color(&it.kind))),
            ];
            crate::ui::put_spans(buf, x0, y, w, &spans);
        }

        self.render_show(area, buf);
        self.render_editor(area, buf);
    }

    fn render_show(&self, area: Rect, buf: &mut Buffer) {
        let Some(show) = &self.show else { return };
        let bw = area.width.saturating_sub(6).clamp(50, 100);
        let bh = area.height.saturating_sub(4).clamp(8, 30);
        let bx = area.x + (area.width.saturating_sub(bw)) / 2;
        let by = area.y + (area.height.saturating_sub(bh)) / 2;
        crate::ui::draw_box(buf, bx, by, bw, bh, &show.title);
        let view_h = (bh as usize).saturating_sub(3);
        for (i, line) in show.lines.iter().skip(show.off).take(view_h).enumerate() {
            crate::ui::put_str(
                buf,
                bx + 2,
                by + 1 + i as u16,
                line,
                (bw - 4) as usize,
                Style::default().fg(Color::Cyan),
            );
        }
        crate::ui::put_str(
            buf,
            bx + 2,
            by + bh - 1,
            " j/k scroll · q/Esc close ",
            (bw - 4) as usize,
            Style::default().add_modifier(Modifier::DIM),
        );
    }

    fn render_editor(&mut self, area: Rect, buf: &mut Buffer) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let bw = area.width.saturating_sub(6).clamp(50, 100);
        let bh = area.height.saturating_sub(4).clamp(10, 28);
        let bx = area.x + (area.width.saturating_sub(bw)) / 2;
        let by = area.y + (area.height.saturating_sub(bh)) / 2;
        crate::ui::draw_box(
            buf,
            bx,
            by,
            bw,
            bh,
            "new type · C declaration (live in the bn instance)",
        );
        let inner = (bw - 4) as usize;
        // Text area rows: box height minus borders (2), status (1), hints (1).
        let text_h = (bh as usize).saturating_sub(4).max(1);
        // Keep the cursor row visible.
        if editor.row < editor.top {
            editor.top = editor.row;
        } else if editor.row >= editor.top + text_h {
            editor.top = editor.row + 1 - text_h;
        }
        for (vis, line) in editor
            .lines
            .iter()
            .enumerate()
            .skip(editor.top)
            .take(text_h)
        {
            let y = by + 1 + (vis - editor.top) as u16;
            crate::ui::put_str(buf, bx + 2, y, line, inner, Style::default());
            // Draw the block cursor on its line.
            if vis == editor.row {
                let cursor_col = editor.col.min(line.chars().count());
                if cursor_col < inner {
                    let under: String = line
                        .chars()
                        .nth(cursor_col)
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| " ".into());
                    crate::ui::put_str(
                        buf,
                        bx + 2 + cursor_col as u16,
                        y,
                        under,
                        1,
                        Style::default().add_modifier(Modifier::REVERSED),
                    );
                }
            }
        }
        // Status line (validation feedback).
        let status_style = if editor.ok {
            Style::default().fg(Color::Green)
        } else if editor.status.starts_with('✗') {
            Style::default().fg(Color::Red)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        crate::ui::put_str(
            buf,
            bx + 2,
            by + bh - 2,
            &editor.status,
            inner,
            status_style,
        );
        crate::ui::put_str(
            buf,
            bx + 2,
            by + bh - 1,
            " Enter newline · Tab indent · ^P check · ^S declare · Esc cancel ",
            inner,
            Style::default().add_modifier(Modifier::DIM),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{rank, TypeEditor};

    fn typed(text: &str) -> TypeEditor {
        let mut e = TypeEditor::new();
        for ch in text.chars() {
            if ch == '\n' {
                e.newline();
            } else {
                e.insert_char(ch);
            }
        }
        e
    }

    #[test]
    fn inserts_and_reads_back_multiline_text() {
        let e = typed("struct s {\nint a;\n}");
        assert_eq!(e.lines.len(), 3);
        assert_eq!(e.text(), "struct s {\nint a;\n}");
        assert_eq!((e.row, e.col), (2, 1));
    }

    #[test]
    fn newline_carries_leading_indent() {
        // After a line that starts with four spaces, Enter re-indents the next.
        let mut e = typed("    uint32_t a;");
        e.newline();
        assert_eq!(e.lines[1], "    ");
        assert_eq!(e.col, 4, "cursor sits after the carried indent");
        e.insert_char('x');
        assert_eq!(e.lines[1], "    x");
    }

    #[test]
    fn newline_splits_at_the_cursor() {
        let mut e = typed("abcdef");
        e.col = 3; // between c and d
        e.newline();
        assert_eq!(e.lines, vec!["abc".to_string(), "def".to_string()]);
        assert_eq!((e.row, e.col), (1, 0));
    }

    #[test]
    fn backspace_joins_lines_at_column_zero() {
        let mut e = typed("ab\ncd");
        e.row = 1;
        e.col = 0;
        e.backspace();
        assert_eq!(e.lines, vec!["abcd".to_string()]);
        assert_eq!((e.row, e.col), (0, 2), "cursor lands at the join seam");
    }

    #[test]
    fn horizontal_move_wraps_across_line_ends() {
        let mut e = typed("ab\ncd");
        e.row = 1;
        e.col = 0;
        e.move_h(-1); // wrap up to end of previous line
        assert_eq!((e.row, e.col), (0, 2));
        e.move_h(1); // wrap back down to start of next line
        assert_eq!((e.row, e.col), (1, 0));
    }

    #[test]
    fn empty_editor_is_detected() {
        assert!(TypeEditor::new().is_empty());
        assert!(typed("   \n\t").is_empty());
        assert!(!typed("struct s {};").is_empty());
    }

    #[test]
    fn composites_rank_before_primitives() {
        assert!(rank("struct") < rank("int"));
        assert!(rank("union") < rank("named_type_ref"));
        assert!(rank("enum") < rank("function"));
        assert!(rank("function") < rank("int"));
    }
}
