# bn lens — TODO

## Persisting renames to disk (`bn save`)

**Status:** not implemented. The write paths — local rename, **function rename** (`r`), **comments**
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

## Startup latency on large binaries (minor)

On a ~2000-function binary, `Ctx::build` is ~1.2 s because it runs 5 sequential `bn` calls
(`function list` ~330 ms + exports + imports + sections + arch, each paying the ~130 ms Python-CLI
startup — see the "bn-as-CLI" discussion). It's a one-time cost, but could be trimmed by running the
independent calls concurrently, or lazy-loading exports/imports on first use. Not urgent.

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

- **list mode** (default): blocks in address order, each with its instructions and labelled successor
  edges (`├─ true → block_1`), back-edges flagged `↑loop`. Robust at any block count.
- **box-graph mode** (`Space` toggles): the same blocks wrapped in ascii boxes with arrow connectors,
  **size-gated** to ≤ `MAX_GRAPH_BLOCKS` (24) — above that it falls back to the list with a note
  (a fixed character grid can't route a big CFG readably).

`g`/`Enter` on an edge target jumps to that block **in place** (via the block-addr→line `cfg_index`).

**Next step if wanted:** the box-graph mode stacks boxes vertically with labelled arrows; it does *not*
do true 2D edge routing (layered ranks, crossing-minimization). That's the genuinely-messy part and was
deliberately deferred — a real Sugiyama-style layout would be the follow-on.

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

## Exports (public-API) view (done this pass)

**Status:** implemented as the **Exports** view (`exports.rs`, mirrors `imports.rs`). Lists exported
symbols — functions and data globals shown distinctly; `Enter` opens a function export's decompile (a
data export's xrefs), `x` cross-references, `p` peeks who-uses-it. Reachable via the `m` menu or the
new `v` top-level view-cycle.

## Sink classifier — extend coverage (minor)

`imports.rs::sink_category` covers the classic libc set. Consider: `mempcpy`, `stpncpy`, wide-char
(`wcscat`/`wcsncpy`), `alloca`, `realpath`, `getwd`, `syslog`-style format sinks, and glibc-prefixed
names (`__isoc99_sscanf` → normalize `__isoc99_` too). Keep the substring set false-positive-safe.

## Done this pass (for context)

- Write paths: local + **function rename** (`r`), **comments** (`;`), **bookmarks/tags** (`t`).
- Live **refresh** (`^R`/menu, threaded + counting banner).
- **View menu** (clickable title) + **Strings** view + **Imports** (attack-surface) view.
- **Decomp peek** (`p` on a code hotspot → pseudo-C at the use) + Strings/Imports usage popups.
- Popup rendering: opaque panels, vertical clip (no short-pane panic), highlight bar.

## Threaded usage popup (`p`) — minor UX

`p` in Strings/Imports runs `usage::report` synchronously — up to `MAX_FUNCS` (6) `bn decompile` calls
(~1–2 s worst case), a silent UI freeze with no banner. Acceptable/bounded and consistent across both
views, but ideally it'd run off-thread with a spinner like `start_refresh` does. Deferred for
consistency; revisit if the freeze feels bad in practice.

## Deferred / future

- Add items here as they come up.
