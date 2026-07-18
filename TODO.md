# bn lens — TODO

## Persisting renames to disk (`bn save`)

**Status:** not implemented. The write paths — local rename, **function rename** (`n`), **comments**
(`;`), and **bookmarks/tags** (`t`) — all mutate the **live bn instance in-memory** only: instantly
visible to every `bn` command against that instance, but **not written to the on-disk `.bndb`**. So
any annotation is lost if the instance restarts (GC'd, rebooted) or the target is reloaded in the GUI.

**Why it's not auto-done:** `bn save` is ~388 ms on a *small* binary and scales with size (seconds on
firmware). Saving on every rename was the old, slow behavior (~860 ms/rename). See DESIGN.md →
"Mutation (deliberately narrow)".

**What to build — an explicit, deliberate save:**
- A **`W` key** in the viewer/picker → `bn save`, with a status line (`✓ saved` / `✗ save failed`).
- **Dirty tracking:** mark the session dirty on the first rename; show a subtle indicator in the header
  (e.g. `●` or `*unsaved*`) so the user knows there are un-persisted changes.
- Consider **save-on-quit** prompt (or auto-save on `q` if dirty) — decide vs. explicit-only.

**Where it saves:** `bn save` defaults to `<filename>.bndb`. For the dogfood targets that's the global
cache, e.g. `~/.cache/bn/bndb/<name>.bndb`. A `bn save <path>` can redirect.

**Open question:** is disk persistence even in-scope for the lens, or should it stay live-in-instance
and leave `bn save` to the launching agent? (Revisit — tied to the "bn-as-CLI vs. persistent-bridge"
transport discussion.)

## Startup latency on large binaries (done)

**Status:** implemented. `Ctx::build` keeps the two prerequisite reads sequential — `target_info` (the
session-liveness gate) then `functions` (must be non-empty) — and fans out the remaining four
independent reads (symbols, data-symbols, imports, sections) concurrently via `std::thread::scope`.
Measured on a 2906-function target: serial sum ≈ 0.9 s → ≈ 0.6 s. `Bn` is `Sync` (its shared failure
state is an `Arc<Mutex>`), so scoped threads share `&bn`; the four `Result`s apply via `?` in order.

Keeping the prerequisites ahead of the fan-out (per an adversarial review) preserves the sequential
**fail-fast**: a dead/dying session errors at `target_info` immediately rather than blocking on a
concurrently-hung bulk read (`Command::output()` has no timeout). The four fanned-out reads are only
reached once the session is known live — the sequential path would have called all four there anyway,
so concurrency changes their latency, not liveness.

**Possible follow-ups:** subprocess-level timeouts on `bn` calls would harden the remaining edge (a bulk
read hanging after another errored); lazy-load exports/imports on first view use to trim further.

## Navigation / session persistence (side-parked)

**Status:** not implemented. The picker's `recent` list (your `▸` opens and the agent's `◆`
references) and any per-session navigation trail live **in-memory only** and die with the pane. For a
multi-session RE effort, a small on-disk breadcrumb per instance/target — the "▸ you" trail, and
maybe a notes/marks list — would let the shared map survive a relaunch.

**Open questions:** where it lives (alongside bn's session state under `~/.cache/bn/`? a lens-owned
file keyed by instance+target?), what's worth persisting (opens only, or marks/notes too), and how it
reconciles when the underlying `.bndb` changed between sessions. Tied to the same
"live-in-instance vs. persistent" question as the `bn save` note above.

## Control-flow graph view (done this pass — box-graph mode can go further)

**Status:** implemented. A per-function **CFG view** is now a fourth viewer view (`v`/`i` cycle
Decomp → MLIL → Disasm → **CFG**). It walks `func.basic_blocks` + typed `outgoing_edges` via
`bn py exec` (there is no first-class CFG command; large output goes through `--out` to dodge the
stdout spill envelope — see `bn.rs::cfg` + `CFG_PROGRAM`). Rendering lives in the pure, tested
`cfg.rs`:

- **graph mode** (default, `Space` toggles): a **true 2D layered box-and-arrow layout** — blocks
  ranked into layers (longest path from entry), ordered within a layer by barycenter to reduce
  crossings, connected by orthogonally-routed arrows drawn on a char canvas with junction-merging.
  Long edges thread **dummy columns** (never cross a box); **back-edges/loops** route up dedicated
  right-margin lanes with a `◀` head. Compact nodes (id · addr · terminator) keep it inside the pane
  width; it re-lays out on resize and falls back to the list if it can't fit (or > `MAX_GRAPH_BLOCKS`).
  It is a **spatial navigator, not a text buffer**: `hjkl` move the selected block (highlighted; the
  view scrolls to keep it visible), a click selects a box, and **edges are colour-coded** — green
  (true), red (false), blue (any other/unconditional) — so no true/false text labels are needed.
  `Enter`/`g` drops into the list scrolled to the selected block to read its instructions.
  Implementation: `cfg::graph()` returns a `GraphData` (char grid + parallel colour grid + block
  rects); the viewer has a dedicated colored renderer (`render_cfg_graph`) + `cfg_move`, bypassing the
  generic line renderer.
- **list mode**: blocks in address order with full instructions and labelled successor edges
  (`├─ true → block_1`), back-edges flagged `↑loop`. Scales to any block count; `g`/`Enter` on an
  edge target jumps to that block in place (via `cfg_index`).

**Possible follow-ups:** the router handles adjacent-rank + dummy-threaded forward edges and
right-lane back-edges well; heavy fan-in/fan-out (large switches) can still cross — a full
crossing-minimization pass and per-node variable widths would sharpen it. hjkl is nearest-box spatial;
a strict edge-following mode (Tab through successors/preds) could be added.

## Call-graph / xref-tree view (side-parked, likely hard)

**Status:** not implemented. This is the *inter*-function view (distinct from the intra-function CFG
above): following calls is one-hop at a time (`g` on a Func hotspot); there is no "who calls this /
what does this reach" overview. A call-tree peek (callers ↑ / callees ↓, expandable, `Enter` to jump)
would fit the navigator framing without drifting toward decompiler parity. `bn` already exposes
`xrefs` and `callsites` to build it from.

**Why it's hard:** rendering an interactive, scrollable, expandable tree in ratatui (cycle handling,
depth limits, lazy expansion to avoid fanning out a whole binary), plus deciding how it composes with
the existing nav stack and hotspot model. Non-trivial UI work; scope carefully before starting.

## Bookmarks / annotations navigation view (done)

**Status:** implemented as the **Marks** view (`marks.rs`) — lists comments + tags/bookmarks, `Enter`
jumps to the annotated function. The read half of the "shared map".

## Exports (public-API) view (done)

**Status:** implemented as the **Exports** view (`exports.rs`, mirrors `imports.rs`). Lists exported
symbols — functions and data globals shown distinctly; `Enter` opens a function export's decompile (a
data export's xrefs), `x` cross-references, `p` peeks who-uses-it. Reachable via the `m` menu or the
new `v` top-level view-cycle.

## Types view + declare (done this pass — extends the write surface)

**Status:** implemented as the **Types** view (`types.rs`). Lists the binary's type system
(`bn types`), composites first; `Enter`/`p` shows a type's layout (`bn types show` — fields +
offsets); `n` opens a **multi-line C-declaration editor** to author a new type. The editor auto-indents
on newline, validates without committing via `^P` (`bn types declare --preview`, showing the parsed
name + `size=`), and commits with `^S` (`bn types declare`).

**Write-surface note:** this *deliberately extends* the previously annotation-only write surface —
adding *whole* user types is now in scope (CLAUDE.md invariant updated). It stays live-in-instance
like every other write (no `bn save`; deferred). **Editing fields of existing structs and graph
editing remain explicit non-goals** — the view reads existing layouts but only *adds* new types.

**Possible follow-ups:** live validation as-you-type (currently on-demand via `^P`; per-keystroke
`--preview` would spawn a bn process each keypress, too slow); load a declaration from a file
(`bn types declare --file`); struct-field editing (`bn struct field`) if that non-goal is ever
revisited.

## Sink classifier — extend coverage + catalog gap-fill (done)

**Status:** implemented. `imports.rs::sink_category` now also covers `realpath`/`getwd` (buffer),
`readlink`/`readlinkat`/`fgets`/`recvmsg` (source), and the `v*`/wide printf family
(`vfprintf`/`vprintf`/`vdprintf`/`wprintf`/`fwprintf`/`vwprintf`/`vfwprintf`) — on top of the earlier
`mempcpy`/`stpncpy`/`wcs*`/`alloca`/`__isoc99_` work. More importantly, the classifier now **supplements
the `bn taint models --present` catalog on a per-import miss** instead of being bypassed whenever a
catalog is present (`resolve_roles`): genuine catalog holes (e.g. `__vfprintf_chk`, a `dm_strncpy`
wrapper) now surface. Provenance is explicit — a heuristic gap-fill renders as a dimmed `?` row with a
`hint:` label and is counted separately in the header (`· N hint`), so a guessed candidate is never
mistaken for a catalog fact. Full heuristic-fallback mode (no catalog) is unchanged (footer already
discloses it globally; rows aren't individually dimmed). Pure `resolve_roles` is unit-tested for
catalog-hit-authoritative vs catalog-miss-hint vs no-catalog behavior.

**Possible follow-ups:** `realpath(path, NULL)` self-allocates (safe mode) so the `hint:buffer` label
slightly overflags that case — acceptable while framed as a hint, but a fortified/arg-aware refinement
could sharpen it. A catalog-side suppression/coverage marker (tombstone) would let the producer mark an
omission as *intentional* so the heuristic defers to it.

## Done this pass (for context)

- Write paths: local + **function rename** (`n`), **comments** (`;`), **bookmarks/tags** (`t`).
- Live **refresh** (`^R`/menu, threaded + counting banner).
- **View menu** (clickable title) + **Strings** view + **Imports** (attack-surface) view.
- **Decomp peek** (`p` on a code hotspot → pseudo-C at the use) + Strings/Imports usage popups.
- Popup rendering: opaque panels, vertical clip (no short-pane panic), highlight bar.

## Threaded usage popup (`p`) — minor UX

`p` in Strings/Imports runs `usage::report` synchronously — up to `MAX_FUNCS` (6) `bn decompile` calls
(~1–2 s worst case), a silent UI freeze with no banner. Acceptable/bounded and consistent across both
views, but ideally it'd run off-thread with a spinner like `start_refresh` does. Deferred for
consistency; revisit if the freeze feels bad in practice.

## List-view Esc / switcher UX (done this pass — dogfood friction fixes)

Fixes for friction found driving the lens via herdr send-keys:

- **Esc no longer quits a top-level list.** On Symbols/Strings/Imports/Exports/Types/Marks, `q` is the
  only quit. `Esc` in Normal mode backs out one step: clear an active filter (Imports also drops
  sinks-only), else return to the Symbols list (`Action::Home`); on Symbols with no filter it's a no-op.
  Search-mode Esc (restore prev filter) and popup Esc (close popup) are unchanged. This stops the
  "dismiss search → Esc → one more Esc kills the pane" footgun agents kept hitting.
- **`i` (switch bn) works from every list**, not just Symbols — same `Action::Switch` path.
- **Switcher type-ahead filter** (`/`): narrows the focused column (instances, or targets) with the
  same UX as the lists; Enter keeps the filter. `Esc` is layered (mirrors the list ladder): in search
  mode it exits search and clears the filter (staying in the switcher); with an already-committed
  filter it clears the filter (staying in the switcher); with no filter it cancels the switcher.
- **Clean target label in the header.** A bndb cache selector (`<name>.<hash>.bndb`) is shown as just
  `<name>` via `ui::clean_target_label`; the short form is still a valid `-t` selector so it stays
  copyable. Non-bndb selectors are shown verbatim.
- Help/footer text updated: `q` quits, `Esc` backs out, `i` switches, on all list views.

## Deferred / future

- Add items here as they come up.
