# bn lens — TODO

## Persisting renames to disk (`bn save`)

**Status:** not implemented. Local rename (`r`) currently mutates the **live bn instance in-memory**
only — instantly visible to every `bn` command against that instance, but **not written to the on-disk
`.bndb`**. So a rename is lost if the instance restarts (GC'd, rebooted) or the target is reloaded in
the GUI.

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

## Deferred / future

- Nothing else committed yet. Add items here as they come up.
