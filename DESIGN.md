# bn lens — a headless Binary Ninja navigator TUI that pair-programs with agents

A herdr plugin. In any pane whose binary has a `bn` session open, one key opens a **split beside your
work** with a fast, read-only navigator over the binary — filter/goto/xref/peek — whose superpower is
**looping the launching agent in**: select a line (or a range) and send it a question. Rust +
`ratatui`/`crossterm`; it shells out to `bn` and `herdr`.

## Why Rust (rewritten from a Python prototype)

The Python/curses prototype got us to "feels right" fast, but curses cost us a recurring class of
crashes (writing the terminal's bottom-right cell panics) and a 238-line god-function. `ratatui`
writes through a `Buffer` that **clips instead of panicking**, so that whole crash class is gone, and
the code is split into small typed modules with unit tests.

## Architecture (`src/`)

| module | responsibility |
|--------|----------------|
| `main.rs` | dispatch: `launch` (action) vs `picker` (pane) |
| `launch.rs` | read herdr context, open the picker split beside the focused pane |
| `app.rs` | terminal setup, event loop, Picker↔Viewer state |
| `ctx.rs` | load-once context: instance (self-healing), functions, data symbols, address map, pane tokens |
| `bn.rs` | `bn` CLI wrappers (functions/exports/decompile/xrefs/read/session-list) + instance resolution |
| `herdr.rs` | `herdr` CLI wrappers (pane read, prompt agent, open pane) + context parse |
| `syntax.rs` | pure pseudo-C tokenizer → `(text, kind)` runs (**unit-tested**; replaces pygments) |
| `theme.rs` | token-kind → colour |
| `help.rs` | global, scrollable `where / key / action` shortcut reference |
| `menu.rs` | the `bn lens` title dropdown: switch view (Symbols/Strings) + global actions |
| `picker.rs` | the **Symbols** list (functions + data): filter, vim nav, colours, mouse |
| `strings.rs` | the **Strings** list: recovered text, filter, `Enter`/`x` to xref its uses |
| `viewer.rs` | code-viewer state model and load lifecycle |
| `viewer/actions.rs` | navigation and data-backed actions: goto/peek/xrefs/rename/comment/tag |
| `viewer/input.rs` | keyboard/mouse modes, search, view cycling, and agent asks |
| `viewer/render.rs` | wrapped code rendering, hotspot styling, and modal layouts |
| `viewer/stack.rs` | stack-frame model, navigation, and responsive panel/modal rendering |
| `viewer/hotspots.rs` | pure token promotion, identifier validation, and dump symbolization |
| `switch.rs` | ranger-style instance/target switcher (miller columns + live target-info preview) |

Data flows one way: `Ctx` is built once and shared by ref; the picker returns an `Action`
(`OpenDecompile`/`OpenXrefs`/`Quit`); the viewer returns `Exit::{Stay,Back}`. No global mutable state.

## Features

**Picker** — every function in **address order**, under an `── all functions · by address ──`
delimiter, with a **`── recent ──`** subsection on top: functions you opened here (`▸ you`) and
functions/addresses the agent named in its pane (`◆ agent`, `★ both`), newest first. The recent group
is a shortcut — every function still appears in the by-address list below. It refreshes live (your
opens immediately; the agent scan on the ~1s poll). Non-function addresses are annotated with their
**section + nearest symbol** (`0x4152a0  .bss → __bss_start`) instead of a bare `(addr)`. Vim nav
(`j/k/g/G/^D/^U`) skips the delimiters; `Enter` decompile, `x` xrefs, `s` sections, mouse wheel/click.
`/` search filters the full list live and ranks best-match first (`↑`/`↓` pick, `Enter` opens the top
hit, `Tab` commits the filter to the list); the recent subsection shows only when unfiltered.

**Views + the title menu** — the list pane has two top-level views: **Symbols** (functions + data, the
default `picker.rs`) and **Strings** (`strings.rs`, an address-ordered list of recovered text where
`p` peeks a **usage popup** — it parses `bn xrefs` on the string's address, decompiles each
referencing function once (`--addresses`), and shows the **pseudo-C statement** at each callsite
(grouped by function; falling back to the disassembled instruction when a site maps to no decompiled
line), plus any data refs; `Enter`/`x` opens the full navigable xrefs listing instead). Clicking the
` bn lens ` title (or `m`) opens a small **dropdown** (`menu.rs`) that switches view and reaches the
global actions (Refresh, Switch bn, Help, Quit); a click on the title toggles it, a click on an entry
or click-away dismisses it. `app.rs` owns an `AppView` enum and routes keys/mouse/render to the active
list; the Strings list is built lazily on first switch and re-pulled by the same refresh path as the
picker. Views share the `Viewer` for anything they open.

**Sections** — `s` (in the picker or viewer) opens a scrollable popup of the `bn sections` table
(address range, size, perms, semantics, name) with a `w+x` summary line up top — quick orientation and
a cheap security signal (any writable+executable section). Lazily fetched and cached.

**Viewer** — header shows the function's address; syntax-highlighted, **wrapped** decompile/xrefs
(long lines wrap to a `↳` gutter that carries the selection bar). A `▸` **line cursor** (`j/k`) plus a
per-token **hotspot** cursor. The tokenizer types every word; a second pass (`build_spans`) promotes
the interactive ones into typed **hotspots**: **Func** (blue; in `func_names`) → goto/xrefs; **Data**
(cyan; exports/imports/`addr_by_name`/`data_<hex>`) → peek/xrefs; **Addr** (yellow; a `0x…` that lands
inside a mapped section, via `ctx.section_ranges`) → peek, or goto if the section is executable;
**Local** (gray; from `bn local list`) → `p` shows its type, all its occurrences highlight while it's
selected, and **`r` renames it** (see below); **Str** (magenta; any string literal) → `p` peeks the
backing bytes and `x` xrefs the string, both after resolving the content to its address via a lazy
`ctx.strings()` map (which prefers the real `.rodata` copy over the `.dynstr`/`.symtab` duplicates; the
escape rendering matches the decompile, so even a multi-line literal resolves). Constants/offsets (a
`0x…` in no section, like `hif + 0x120`) stay inert. **`Tab`/`Shift-Tab` step hotspot-to-hotspot**
(granular — both calls in `f(g(x))` are reachable), **click** selects one, and `g`/`Enter`/`p`/`x`/`r`
dispatch on its kind. Peek resolves internal symbols on-demand via `bn xrefs` and **symbolizes the hex
dump** (`+off→name` for any 8-byte value that is a known symbol address). `/`+`n`/`N` find in function,
`b` back (nav stack), `q` to the list.

**Stack inspector** — `S` consumes structured `bn local list --format json` data rather than parsing
decompiler text: stable local IDs, full types, stack/register provenance, signed frame offsets, and
`span_to_next` slot spans. Slots render high-to-low with saved/compiler entries dimmed and overlapping
offsets grouped. The selected code local seeds the stack selection; stack selection highlights all
rendered uses, `Enter` jumps to the first use, and `r` reuses local rename. At 120+ columns it is a
side panel beside the code; narrower terminals get a centered modal. Slot span is labeled separately
from type size because recovered spacing can include alignment/padding.

**Decomp peek** — `p` on a **code** hotspot (a function name, or a `0x…` in an executable section —
notably a callsite line on the xrefs page) opens a scrollable pseudo-C popup of the containing
function, centered on and highlighting the statement at that address. It decompiles via
`bn decompile <addr> --addresses --format json` (bn resolves an interior address to its function and
returns the name); `decomp.rs` maps the use address to its statement (exact, else nearest at/below) and
normalizes the address-column indentation. Data hotspots still peek as a symbolized byte dump. This is
the same address→pseudo-C mapping the Strings usage popup uses.

**Views** — `i`/`I` cycle the current function through **decompile → MLIL → disassembly** (forward /
back; `bn decompile` / `bn il --view mlil` / `bn disasm`). The three code views + the xrefs view are a
single `View` enum; decompile uses the pseudo-C tokenizer, the rest the plain one, and hotspots build
over all of them (so a branch target in disasm is a clickable address). **mlil/disasm use a muted asm
palette** (`theme::asm_style`) instead of the pseudo-C one — addresses & byte columns dim, `0x…` cyan,
mnemonics/registers plain, hotspots on top. Two tokenizer tweaks make this clean: the plain tokenizer
consumes whole hex runs (so `0043274c` isn't split at `a-f`), and a 2-char/≥5-char all-hex identifier is
tagged as a value (dims bytes/addresses uniformly) while 3–4-char ones stay names (so `add`/`adc`
aren't dimmed). The instance/target switcher is
now **picker-only** (`i` there); the viewer's `i` is view-cycling. **Popups dim the backdrop** — while
any popup is open the code renders dimmed and un-highlighted, so nothing "spills" around the box.

**Global help** — `?` is intercepted above every picker, viewer, popup, search, and switcher mode and
opens one scrollable shortcut guide. Status bars stay compact and context-specific. While composing
an agent question, `?` remains literal punctuation.

**Ask the agent** — `a` on the cursor line, or `V` to visual-select a range then `a`, opens a modal.
The message is a single line: `[bn lens] -i <inst> -t <target> · <fn> @ <addr> · lines <lo>-<hi> ·
code: <code…> · [user] <question>` — a copy-pasteable locator + address anchor (so the agent can
re-query and pull its own context) plus the highlighted code and the question. One line by necessity:
an embedded newline is a submit to `herdr pane run`.

**Exact-or-nothing delivery.** The ask channel is *fail-closed*: it sends only to the pane the lens
was spawned from (`BN_LENS_PANE`, captured at launch from the focused pane), and only if that pane
still hosts the **same** agent — verified by session id (`BN_LENS_AGENT_SESSION`, captured at launch).
No launching pane / agent gone / agent replaced → it refuses and says so, never guessing a recipient.
(Contrast the bn *instance*, which auto-resolves to the newest live one: a wrong disassembly is
harmless, but a wrong ask *recipient* leaks real target names.) The destination is shown in the header
(`◐ → <pane> <agent> <status>`) and in the ask dialog, so a mis-wire is visible before you send.

**Instance resolution** — `BN_LENS_INSTANCE` → cwd `.bn-<id>` marker → single live → newest-started
live; **self-heals** past a stale marker that points at a functionless instance. Big reads use
`bn … --out <file>` to dodge bn's stdout spill envelope.

## Build / install

```
cargo build --release        # produces target/release/bn-lens
herdr plugin link /opt/bn-tui
```

The manifest's `[[build]]` runs `cargo build --release` on GitHub install; a **locally-linked** plugin
needs the binary pre-built (as above), since local linking skips build steps.

## Tests

`cargo test` — unit tests on the pure logic (the tokenizer: keyword/type/name classification, line &
block comments, hex/number handling, plain-text address/name tokenizing). The TUI itself is dogfooded
in a herdr pane.

## Mutation (a narrow, annotation-only surface)

The lens was read-only by construction until **local rename**; the write surface is now four
annotation actions, all live in the bn instance, none auto-saved:

- **`r` rename** — context-aware. A selected **Local** hotspot renames the local; a selected **Func**
  hotspot (or, with no useful hotspot, the function in view) renames the **function**. The new name is
  validated as a C identifier (spaces/invalid rejected inline).
- **`;` comment** — targets a concrete address when one is resolvable (a selected Addr hotspot, or the
  address leading a disasm/MLIL line), else the current function's doc comment (`bn comment set
  --function`). After a set, the current view reloads so an inline note renders.
- **`t` bookmark** — a `Bookmarks` tag (with an optional note) on the address or function.

Each shells out to a `bn … --summary` mutation and checks `"success": true`. All mutate the **live bn
instance in-memory** — immediately visible to every `bn` command against that instance — and **none
call `bn save`**: persisting to the on-disk `.bndb` is a separate, deliberate step (388 ms+, scales
with binary size; see `TODO.md`).

**Two apply strategies.** A **local** rename is just an identifier swap, so tokens/locals/spans are
retexted in place (`apply_local_rename`, no re-decompile) — ~200 ms vs. the old ~860 ms. A **function**
rename changes name maps and every callsite, so it can't be patched locally: the viewer returns
`Exit::Reload` and the app rebuilds `Ctx` and reloads the viewer. `^R` triggers the same ctx rebuild
manually, so edits the **agent** made to the shared instance (renames, new symbols) show up on demand
— `Picker::refresh` re-syncs the function list while preserving the recent/opens history.

The rebuild (~1s of sequential `bn` calls) runs on a **worker thread** so the UI keeps drawing: the
event loop shows a centered bottom banner counting elapsed seconds, ticks at 100 ms for a smooth
counter, and swallows input until the new `Ctx` arrives over a channel and is swapped in. The current
function's own signature name is a `Func` hotspot too (so it's click/`x`/`r`-able); `act_primary`
no-ops the degenerate self-goto.

## Non-goals (kept deliberately)

Not a Binary Ninja clone — the justification is *headless* + *agent pairing*, not decompiler-feature
parity. The write surface stays limited to **naming and annotation** (rename / comment / bookmark);
resist creep toward types/structs/graph editing.
