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

## Call-graph / xref-tree view (side-parked, likely hard)

**Status:** not implemented. Today following calls is one-hop at a time (`g` on a Func hotspot); there
is no "who calls this / what does this reach" overview. A call-tree peek (callers ↑ / callees ↓,
expandable, `Enter` to jump) would fit the navigator framing without drifting toward decompiler
parity. `bn` already exposes `xrefs` and `callsites` to build it from.

**Why it's hard:** rendering an interactive, scrollable, expandable tree in ratatui (cycle handling,
depth limits, lazy expansion to avoid fanning out a whole binary), plus deciding how it composes with
the existing nav stack and hotspot model. Non-trivial UI work; scope carefully before starting.

## Bookmarks / annotations navigation view (next up)

**Status:** not implemented. You can now *create* tags/bookmarks (`t`) and comments (`;`), but there's
no way to *list and jump between* them — the write half of the "shared map" exists without the read
half. A menu view over `bn tag list` (and/or `bn comment list --format json`) would let you (and the
agent) navigate marked spots: `Enter` jumps to the tagged/commented address in the viewer. Fits the
existing view/menu architecture (mirror `imports.rs`). High value for the pairing loop.

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

## Deferred / future

- Add items here as they come up.
