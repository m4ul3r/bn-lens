# bn ↔ bn lens — interface design & performance audit

**Date:** 2026-07-22 · **Scope:** how `bn lens` (this repo) talks to `bn` (`/opt/bn`), for the goal of
a human and an AI agent **pair-reversing** a binary side by side in herdr panes.

This document is the *interface* design record. `DESIGN.md` remains the bn-lens architecture doc and
is unchanged by this audit. Nothing here has been implemented — it is a decision record plus a ranked
plan.

All numbers are **measured** on a live bridge against a 5,032- and a 6,523-function target unless
explicitly marked UNMEASURED. Measurement method and raw data are in Appendix A. Per the repo
sanitization rule, no target names, paths, instance→target mappings, or real symbols appear here.

---

## 0. TL;DR

- A `bn` CLI invocation costs **~127 ms of Python startup** before any work happens. For a cheap read,
  that is **99.3% of the call**. The same data over the raw AF_UNIX socket costs **0.64 ms**.
- The lens pays that floor 2× to open a function, 6× to cycle views, up to 19× on a `p` peek, and 7×
  on every startup and every `^R`.
- Three lens read paths go through `bn py exec`, which is **write-locked** — they exclusively lock the
  instance the paired agent is reading from.
- Five plausible-looking optimizations were measured and are **worse than the status quo**. They are
  recorded in §5 so nobody re-proposes them.

---

## 1. How the two sides actually connect

```
  bn lens (Rust)                    bn (Python CLI)              bn_agent_bridge (in-process BN)
  ──────────────                    ───────────────              ──────────────────────────────
  Command::new("bn")   ──fork──▶    ~127 ms startup
    + ["-i", inst]                  argparse → params
    + ["-t", target]                                 ──AF_UNIX──▶  @op binder → BinaryView
    + ["--out", tmp]                formatters.py  ◀──JSON line──  {"ok":true,"result":…}
  read(tmp); unlink(tmp) ◀──────    text or JSON
```

### The wire protocol (verified against source)

- **Socket:** AF_UNIX SOCK_STREAM at `~/.cache/bn/instances/<id>.sock`, mode `0o600`.
- **Request:** one JSON object + `\n` — `{"id", "op", "params", "target"?}` (`transport.py:464-472`).
- **Response:** one JSON object with **no trailing newline and no length prefix** — the client reads to
  EOF, never `read_line` (`transport.py:486-490`). Envelope is `{"ok", "result"|"error"}`.
- **One request per connection.** `handle()` does a single `readline` then returns
  (`bridge.py:702-770`); a second request on the same socket is provably dropped.
- **Auth:** SO_PEERCRED, same-uid only. No handshake, no token.
- **Cancellation:** the client sets a per-request timeout and fires `cancel_request` **on a separate
  connection** when it trips (`transport.py:433-451`).
- **Versioning:** none on the wire. The registry JSON carries `plugin_version` / `plugin_build_id`, so a
  client can version-gate *before* connecting. `grep schema_version` across `/opt/bn/src/` returns
  exactly one hit, inside `taint_models.json`.

### The op binders are pure pass-throughs

`bridge.py:2915-2924` reads `params["identifier"]` straight off the wire. Nothing is computed
CLI-side. Validation lives at the **op layer**, not the CLI layer (`_shared.py:107,128`), and the
bridge is explicitly hardened for non-CLI callers — actual comment at `bridge.py:3153`:

> `limit=None means "no limit" -- match the imports/sections binders so a raw-socket / py exec caller
> that omits limit gets every string (#122)`

**A Rust socket client is a supported, tested-for path, not a hack.** That is the single most important
finding for §6.

### The spill envelope is an *agent* affordance the lens inherits by accident

`DEFAULT_SPILL_TOKEN_LIMIT = 10_000` (`output.py:26`) is sized in **LLM tokens**, to protect an agent's
context window. The lens is not an LLM. It works around the envelope with `--out <tmp>`
(`bn.rs:927-966`), paying a file write + read + unlink per big read.

That workaround is currently **load-bearing for a second reason**: `--out` also uncaps the paged
default limit (`cli.py:1286-1296`). See N1/N2 — two calls escaped it and are silently truncating.

---

## 2. The headline problem

**99.3% of a `bn` call is Python interpreter startup.**

| measurement | result |
|---|---|
| bare CPython 3.14.2 start | 16.6 ms |
| `+ import bn.cli` | 74.5 ms |
| `bn --version` (never opens a socket) | **126.8 ms** |
| `bn sections` via CLI (5,688-byte payload) | **127.6 ms** |
| same `sections` over the raw socket | **0.64 ms** |

Cost to the lens, per user action:

| action | `bn` spawns | wall | actual bridge work |
|---|---|---|---|
| open one function (`decompile` + `local list`) | 2 | 540 ms | ~4 ms |
| cycle decomp → MLIL → disasm | 6 | ~800 ms | ~15 ms |
| `p` peek, worst case (`usage.rs:11-12`) | 19 | ~3.6 s | ~50 ms |
| `Ctx::build` (startup **and** every `^R`) | 7+ | ~1.45 s | ~660 ms |

**Caveat that matters:** `target info` costs ~59 ms of genuine bridge work (confirmed independently by
raw socket probe). "Bridge work is free" is true for cheap reads only. `decompile`, `xrefs`, and
`taint` were **not measured raw** — see §7.

### Close second: the lens takes the write lock on read paths

`py_exec` is registered `lock="write"` (`bridge.py:3245`) and takes both `_write_gate` and the
exclusive `_target_lock.write()` (`bridge.py:1093-1097`). The lens uses it for `CFG_PROGRAM`
(`bn.rs:555`), `DATA_MAP_PROGRAM` (`:593`), and `DATA_SYMBOLS_PROGRAM` (`:642`).

Reads are genuinely concurrent — `ThreadedUnixServer` (`bridge.py:770-773`), and `_dispatch_on_main` is
a bare binder call with no main-thread queue (`bridge.py:1204-1208`). So **the lock is the concurrency
control**. Opening a CFG genuinely stalls the agent's in-flight taint run, and `_ReadWriteLock` is
writer-preferring, so it blocks *new* agent reads the instant it queues.

This fires on: CFG open, Data view open, data-address peek, **every startup, and every `^R`**.

---

## 3. What the current interface gets right

These are load-bearing properties. Any change must preserve them.

1. **Ask delivery is fail-closed, and the asymmetry is reasoned.** `ask_key` (`viewer/input.rs:790-816`)
   re-verifies the agent at *send* time — not from the 1 s poll cache — refusing on empty pane, no
   detected agent, or session mismatch. `same_agent_session` (`herdr.rs:83-85`) is pinned by a unit test
   (`:163-169`). `DESIGN.md:152-158` states the reasoning: a wrong bn *instance* is harmless, a wrong
   ask *recipient* leaks real target names.
   **Known hole:** `expected.is_empty()` fails **open** — if launch captured no session id, any later
   agent in that pane receives asks.

2. **The ask is a re-query seed, not a data dump.** `Viewer::locator` (`viewer/input.rs:722-738`) emits
   `[bn lens] -i <inst> -t <target> · <fn> @ <addr>` — a literally paste-able bn command prefix.
   Second-order benefit the design got for free: **that line is itself the `-t` scope marker the pane
   scan looks for**, which is why `ctx.rs:497,518-528` uses a lens locator as its test fixture. The
   channel self-seeds.

3. **Three deliberate latency tiers for three blast radii.** Local rename → `Exit::Stay` +
   `apply_local_rename` retexts in place with no re-decompile (`viewer/actions.rs:293-324`).
   Comment/tag → `Exit::ReloadView` (viewer + marks only). Function rename → `Exit::Reload` → full ctx
   rebuild, because name maps and callsites must update. **The cache work in §4 must not flatten this.**

4. **`HealthScope` — a failed backend read is visible, not silent.** `run_*_state_*` records into a
   shared `Arc<Mutex<Option<CommandFailure>>>` surfaced in the status bar; plain `run_*` report locally;
   only a successful retry of the *same command family* clears it (`bn.rs:898-974`, `app.rs:430-460`).
   For a tool reading a live, externally-mutable instance this is the right property, and it is the
   reason "stale cache" is diagnosable at all.

5. **`--out` doing double duty, exploited correctly.** It bypasses the spill envelope *and* uncaps the
   paged limit. The per-call unique temp name (`bn-lens-<pid>-<seq>.out`, `bn.rs:733-741`) is what makes
   the refresh worker safe against a concurrent foreground read.

6. **bn's bridge lock design is correct, and the lens mostly respects it.** `refresh` and `load_binary`
   are `lock="none"` and run their multi-minute `update_analysis_and_wait()` *without* the read-blocking
   lock (`bridge.py:1298-1345,1711-1742`), so a client can poll during a load. `_ReadWriteLock` is
   writer-preferring so a reader stream cannot starve a queued write (`bridge.py:219-257`). The lens's
   one violation is reaching for `py_exec` (§2, and N-item X4).

---

## 4. Ranked plan

Ordered by win/cost within each group. Every item survived adversarial verification against source;
items marked **REWORK** carry the required correction inline.

### 4.1 Do now — under a day each

**N1. Delete `--limit 5000` from the five paged loops.** — *1 hour*
`bn.rs:1078` (function list), `:1169` (imports), `:1224` (types), `:1306` (classes), `:1345` (exports).
All five already run through `run_out_*`, so dropping `--limit` makes `_effective_limit` return `None`
and the bridge returns the whole body in one call. Keep the `has_more` loop as a safety net — with
`limit=None` it goes false on the first iteration.
**Win (measured, 5,032 functions):** two pages at 0.79 s + 0.81 s = **1.60 s → 0.79 s**. Scales
linearly with function count. Comes off every `Ctx::build` and every `^R`.
Do **not** raise the limit to some larger number — *dropping* it is strictly correct and needs no magic
constant.

**N2. `sections_checked` and `target_info` must use `--out`.** — *10 minutes*
`bn.rs:1552` is `run_state_checked(&["sections"])` — no `--out`, no `--limit`. `sections` is
`paged=True` (`/opt/bn/src/bn/commands/misc.py:186-215`) and `_effective_limit` returns **100** without
`--out` (`cli.py:1286-1296`). A target with >100 sections silently loses ranges, degrading `section_of`,
`string_rank`, the Addr-hotspot in-section test, and the code-vs-data decision that routes `g` to
decompile vs the data view (`ctx.rs:59-97,120-133`; `viewer/hotspots.rs:320-332`).
Change to `run_out_state_checked`. Same for `target_info` (`bn.rs:1536-1547`).
**Risk:** none. **UNMEASURED:** whether the dogfood target actually exceeds 100 sections — unlikely for
ordinary ELF, plausible for `-ffunction-sections` static builds and firmware.

**N3. Viewer `s` should read `ctx.sections_text`.** — *10 minutes*
`viewer/input.rs:457-466` calls `ctx.bn.sections()` on every keypress; `ctx.rs:277` already holds the
identical lines. ~130 ms per press.

**N4. Share `Ctx`'s already-fetched payloads with the views that refetch them.** — *half a day*
`Ctx::build` runs `exports_list_checked` (`ctx.rs:240`) and `imports_list_checked` (`:276`);
`ExportsList::build` (`exports.rs:90-94`) and `ImportsList::build` (`imports.rs:230`) then re-run the
identical sweeps on first view open. `bn strings` runs twice for the same bytes — `StringsList::build`
(`strings.rs:102`) and `Ctx::strings`'s `OnceCell` (`ctx.rs:137-153`).
Store the raw `Vec<Export>` / `Vec<Import>` / raw strings vec on `Ctx` and project from there.
**Win (measured):** Exports first-open 129 ms → 0; Imports 302 ms → 175 ms (taint models only); Strings
paid once instead of twice.

**N5. Gate the ask on agent status.** — *1 hour*
`ask_key` (`viewer/input.rs:790-816`) already has `agent` in hand from the `pane_agent` call at `:801`
and never inspects `agent.status`, though `draw_partner` renders it (`app.rs:384-410`). When status is
`working`/`blocked`, show ` ⚠ agent is working — Enter again to send anyway` and require a second Enter.
Override, not a hard block.
**UNMEASURED:** whether `herdr pane run` queues or drops at a busy agent. The gate is correct either way.

**N6. Record §5 (the refuted optimizations) where the next person will read it.** — *done, this file*
Four measured dead ends. Highest win/cost item on this list — prevents an estimated 1–2 weeks of work.

### 4.2 Do next — 0.5 to 3 days each

**X1. Progress splash before `Ctx::build`.** — *half a day*
`app.rs:717` calls `Ctx::load()` **before** raw mode is entered at `:735-737`. The terminal is blank for
~1.45 s. Worse on the retry path: `Ctx::load` (`ctx.rs:157-203`) retries per candidate instance, each
~1.0–1.1 s before the early `Err` at `ctx.rs:230-232` — with 3 viable candidates that is ~3 s of dead
terminal.
Enter raw mode + alt screen first, paint `bn lens · resolving instance…`, thread a
`Sender<&'static str>` step label through `Ctx::build`, redraw with an elapsed counter.
**Win:** time to first frame 1,450 ms → ~30 ms. Purely perceptual; wall time to interactive unchanged.
**Risk:** terminal teardown on the `Ctx::load` error path (`app.rs:718-726`). Test with
`BN_LENS_INSTANCE` pointed at a dead id.
*Note:* price this standalone — it is **not** free-if-bundled with a parallel `Ctx::build`, because that
is refuted (§5.1).

**X2. Dirty-flag lists instead of refetching on refresh.** — *half a day*
`poll_refresh` (`app.rs:285-328`) installs the new `Ctx` then, still on the UI thread with
`self.refreshing` still `Some` (so the banner cannot tick), synchronously calls `refresh()` on six lists
plus `viewer.reload()`. Measured: strings 295 + imports 302 + exports 129 + classes 167 + types 136 +
marks 286 + viewer reload ~540 = **~1.76 s of frozen UI after the worker returns**. Separately,
`set_view` rebuilds `MarksList` unconditionally on every visit (`app.rs:187`) — 286 ms every time you
cycle past Marks.
**REWORK — the original proposal named the wrong edit site.** The dirty-insert cannot live in
`viewer/input.rs:273-283,324-325`; those handlers take `&Ctx` and return `Exit`, with no `App` access.
The hook is the `Exit::ReloadView` arm at **`app.rs:580-592`**, which *already* eagerly calls
`marks.refresh(&self.ctx)` on every comment/tag commit. **That eager call must be deleted**, or the
286 ms simply moves from `set_view` to the commit keystroke.
**Also:** `AppView` derives only `Clone/Copy/PartialEq` (`app.rs:39-48`) — needs `Eq + Hash`.
**Scope down:** leave `viewer.reload()` on the UI thread. Moving its fetch into the worker is
under-specified — `Viewer::reload` contains an external-rename self-heal that must resolve `self.name`
against the *new* ctx (`viewer.rs:330-348`), and `View::Cfg`/`View::Data` fetch no linear text, so
`Option<String>` is the wrong worker payload.
**Win:** ~1.76 s → ~540 ms post-worker; every Marks revisit 286 ms → 0.

**X3. `Job<T>` + spinner; route `apply_switch` and `p` through it.** — *1–1.5 days*
`apply_switch` (`app.rs:114-141`) runs a full `Ctx::build` **inline on the UI thread with no banner** —
the most expensive user action in the app and the only expensive one with zero feedback. `usage::report`
blocks up to 3.6 s with a static screen.
Extract `Refreshing`/`draw_refresh` (`app.rs:263-349`) into `Job<T> { started, label, rx }` / `draw_job`.
Route `apply_switch` through it (gaining the Esc hatch it lacks). Run `usage::report` as a job.
**REWORK: drop the parallel fan-out of the 6 decompiles + 12 disasms.** Measured serial 1.325 s vs 6-way
**1.941 s**; each individual decompile inflated 0.21 s → 1.90 s under contention. The fan-out makes the
peek *slower*. **The win here is feedback, not speed.**
**Cost driver:** the `p` popup is currently built synchronously inside each list's `on_key`
(`strings.rs:162`, `imports.rs:335`, `exports.rs:182`); each list must learn to accept a late result.
Also bound in-flight jobs to one — the existing Esc pattern (`app.rs:506-513`) abandons the `Receiver`
and leaks the worker thread.

**X4. Promote the three `bn py exec` programs to first-class read-locked bn ops.** — *2–3 days, both repos*
**The item that most directly serves the pair-reversing goal.** Rationale in §2.
Add `@op("cfg", lock="read")` in `read_decompile.py` (beside `_structured_il`), `@op("data_vars",
lock="read")` and a data-symbol path in `read_misc.py`, with `@command` entries in
`commands/function.py` / `commands/misc.py`. `READ_LOCKED_OPS` is registry-derived (`bridge.py:3396`) so
nothing else needs editing.
**Wins:** removes the exclusive lock from 4 interactive paths plus every startup and every `^R`; removes
the `PyEnvelope` double-parse; converts three `unwrap_or_default()` silent-empties into reportable
`CommandFailure`s.
**Risk (real, will silently break things):** the CFG op must emit **IL instruction indexes**, not
addresses, for `start` and edge `to` at IL levels — `cfg.rs:78-90,152,185-187` keys block identity, edge
routing, **and** back-edge detection on `parse_hex(start)`. Emitting addresses breaks all three quietly.
**Two corrections to the original proposal:** (a) the "400-row cap becomes honest `has_more`" win is
illusory — `show_data_map` requests only a 0x240-byte window (`viewer/actions.rs:434-440`), so 400 rows
is unreachable. The real cost is that `DATA_MAP_PROGRAM` iterates `sorted(bv.data_vars)` over the
**whole view** and filters — O(all data vars) per keypress. (b) Folding data symbols into
`exports --binding all` is not free: `_exports` walks FunctionSymbol *and* DataSymbol, so `binding=all`
floods `addr_by_name` with every local `sub_*`. Needs a kind filter.
**Sequencing:** land `cfg` first — smallest, cleanest, most latency-visible. Decide `data vars`
separately; making its pointer-vs-array heuristic and string-preview length into bn contract is a
one-way door.

**X5. View-content cache.** — *1.5 days*
`Viewer::load` (`viewer.rs:382-443`) re-shells on every entry, and `restore` (`viewer/actions.rs:655-660`)
— used by **both** `^O` and `^F` — calls it unconditionally. `finish_linear_load` (`viewer.rs:445-459`)
re-fetches `local_list` per *view*, not per function. `cfg_cache` is the only memo in the app.
**Win (measured):** decompile 223 ms + local list 319 ms = **540 ms → <5 ms** on every back-nav, `i`/`I`
cycle, and revisit. `xrefs` 388 ms → 0 on re-entry.
**REWORK — the key is incomplete.** `(name, View)` does not cover the address-anchored path.
`viewer.rs:404-432` short-circuits through `decompile_json(0x…)` whenever `focus_addr` is `Some`, and
`Frame` carries `focus_addr` (`actions.rs:630-637`), so `restore` re-enters that branch. **Every `^O`
back to an xrefs- or goto-derived location — the exact pair-RE bounce this exists for — still
re-shells.** Key on the resolved entry address as well.
**Also:** `View` derives only `Clone/Copy/PartialEq` (`viewer.rs:40`) — needs `Eq + Hash`. `View::Data`
is nav-history-bearing and uncovered. There is **no `lru` crate** — `Cargo.toml` has exactly 4 deps
(ratatui, crossterm, serde, serde_json), so hand-roll it or accept a dep on a deliberately lean tree.
**Risk (correctness, not perf) — four invalidation sites.** Own the cache on `Ctx` so `^R` is
structurally safe; `Exit::ReloadView` must evict the current function (comments render *inside*
decompile text); and `apply_local_rename` returns `Exit::Stay` (`input.rs:99-105`) so it has **no
existing invalidation hook** — it must gain a cache writeback. Unit-test each.

**X6. `bn xrefs --format json` — one read replaces the scrape plus 12 `disasm --linear` spawns.** — *0.5–1 day*
`usage::report` (`usage.rs:21-83`) fires 1 + ≤6 + ≤12 = up to **19 sequential subprocesses** and
reconstructs caller→callsite by scraping `code refs` / `data refs` headings (`:129-166`).
`_xref_envelope` (`read_xrefs.py:381-423`) already returns `items[]` with
`caller_function{name,address}` and `context.disasm` (`include_disasm=True`, `:548-566`).
**Win:** 19 → 7 spawns ≈ **1.5 s** off a keypress. Plus `caller_function.address` (today discarded at
`usage.rs:149`, so grouping keys on a possibly-ambiguous *name*) and honest `total`/`has_more` instead of
an inferred cap.
**Correction — the mixed-ARM/THUMB argument is REFUTED.** `_disasm_linear` already honors the containing
function's arch via `_linear_decode_arch` (`read_decompile.py:341-351,431-475`), and the lens
disassembles the *callsite* address, so the containing function **is** the referencing function. No
correctness win; the spawn-count and grouping wins stand.
**Two notes:** pass no `--limit` (text mode never forwarded paging, JSON mode does —
`function.py:473-486`), and `xrefs_checked` is currently `HealthScope::Local` — routing the new call
through `_state` is a **behavior change** to the health cell, not a preservation of it.

**X7. `bn sections --format json` (full version, after N2).** — *half a day*
Deletes ~40 lines of positional column guessing in `parse_section_ranges` (`ctx.rs:59-97`) in favor of
named fields (`read_misc.py:601-625`). **Keep the semantics-first precedence** — bn's own comment at
`read_misc.py:617-624` notes `executable` is *segment*-derived, so `.rodata` inside an r-x load segment
reports `executable: true`, exactly the false positive the current heuristic guards against.
**Gap the original missed:** `sections_text` is not just row lines. `_render_sections_text`
(`formatters.py:2655-2672`) **prepends a `w+x:` verdict line** computed over the full section set, and
the popup title advertises it (`input.rs:459`). Reconstruct it from
`wx_verdict`/`writable_executable_items` or the security signal silently disappears.

**X8. `bn class show --format json` → navigable rows.** — *1–2 days*
`class_show` (`bn.rs:1326-1332`) reads TEXT into a scroll-only `Vec<String>` (`classes.rs:106-140`),
discarding vtable slots with `method{name,address,display_name}`, `size{value,source,at}` provenance,
RTTI bases, and `instances{construction_sites,stored_globals}` (`read_class.py:521-728`) — every one an
address the lens currently renders as inert text.
**Live bug this fixes:** `class show` exits 2 on ambiguity (`cpp_class.py:49-60`), and
`run_out_checked` errors on any non-zero status, so the lens today renders `✗ …` and **discards
`matches[]` entirely**. Render it as a picker.
**Correction:** the "N pages → 1" win is near-zero — `--limit 5000` with a `has_more` break means any
target under 5,000 classes already makes one call. Dropping `--limit` is still correct and free; sell it
as removing a latent multiplier.
**MEASURE FIRST (UNMEASURED):** `_class_show` calls `_build_class_registry`, which demangles **every
function in the binary** (`read_class.py:219-250`) before any per-class work, on every invocation. If
that exceeds ~400 ms on the dogfood target it must go behind X3's `Job`, not run inline.

### 4.3 Later — needs a spike or a decision first

**L1. Transport: direct AF_UNIX client, OR `bn serve`. Pick one.** — *2–5 days* — see §6.

**L2. bn-side: stop materializing every function to serve one page.** — *half a day + tests*
`_list_functions` (`read_listing.py:323-366`) builds a dict per function — including a demangle via
`_display_name` — for the *entire* filtered set, then sorts, then slices. Measured ladder on a
6,523-function target: limit=1 → 563 ms, limit=100 → 556 ms, limit=6523 → 593 ms, count_only → **62 ms**.
~500 ms (~77 µs/function) is spent producing rows that are thrown away.
Easier than it looks: `_filtered_functions` already sorts by `(start, name)`, so for `sort="address"`
**and `not reverse`** pre-slicing is provably equivalent. `min_size` filtering already runs on
`functions`, so `total` stays exact.
**Win:** `--limit 100` from 560 ms → ~70 ms. After N1 the *lens* stops hitting this, so this is a win for
the **agent** and any future paged consumer. `_search_functions` gains much less — it already demangles
every filtered function as part of the match predicate.

**L3. SSA lens on locals — `dataflow defuse` from a Local hotspot.** — *1–2 days*
`HotKind::Local` is the only hotspot kind with no action; `x` on one returns `✗ a local has no
cross-references` (`actions.rs:593-597`). `e` and `T` are genuinely unbound in `normal_key`
(`input.rs:420-487`). Both ops are `lock="read"`.
**REWORK — the plumbing claim is refuted.** `dataflow defuse --var` does **not** accept a `local_id`
despite its help string (`dataflow.py:46-56`). `_defuse` resolves through
`il_format._resolve_ssa_variable`, whose index is keyed strictly by `(SSA variable name, version)`
(`il_format.py:146-172`); a colon-joined `local_id` can never be a key and raises `SSA variable not
found`. **Fix: pass the local NAME** — `Viewer.locals` is already keyed by name, so **no map widening is
needed at all**, which makes phase 1 cheaper than proposed. Taint is fine (`taint_engine._resolve_var` →
`_find_variable_selector`, which does accept `local_id`).
**Also:** `bn taint backward` takes `-f/--function` and a repeatable `--sink`, not a positional +
`--sinks`. And the `^R` worker is not reusable — `Refreshing` is typed `Receiver<Result<Ctx,String>>` and
its banner pauses input app-wide; this needs X3's generic `Job`.
**UNMEASURED:** `taint backward --max-depth 8` cost on a real target. It walks interprocedurally into
callers (`taint_engine.py:4557-4577`) — must go behind a `Job`, never inline. `defuse` is a
single-function lift and is safe inline.
**Do not extend to `taint forward` or `trace` yet:** both need an argument index at a callsite, and
`build_spans` (`hotspots.rs:294-352`) emits flat per-token spans with no notion of argument position.
That is a tokenizer change with its own tests.

**L4. `bn strings --format json`.** — *half a day + the escape work*
Gains `type` and `chars` (today the Strings view cannot tell a UTF-16 literal from ASCII) and lets
`--min-length`/`--section`/`--regex` push server-side. No truncation bug — `Bn::strings` already uses
`run_out_state`.
**REWORK — the load-bearing claim is wrong.** `serde_json::to_string` is **not** equivalent to Python's
`json.dumps(..., ensure_ascii=True)`: serde escapes only `"`, `\`, and control chars and emits non-ASCII
raw; Python escapes *every* non-ASCII codepoint to `\uXXXX`. For any string with a non-ASCII byte the
derived key changes and `Ctx::strings`'s content→address map stops matching — producing exactly the
`✗ couldn't resolve string` regression. Either replicate `ensure_ascii` explicitly, or (better) stop
keying on rendered text at all.
Note the key must match what BN's *pseudo-C decompiler* emits between quotes
(`hotspots.rs:335-346`), **not** what bn's `strings` renderer emits; the two coincide only by assertion
(`bn.rs:1563-1565`).

**L5. Structured focus exchange over herdr pane tokens.** — *~1 day*
`herdr pane report-metadata --source X --token K=V --ttl-ms N` is real, round-trips to
`result.pane.tokens`, merges across sources, and bumps a `revision` change-detector. `poll_status`
already calls `pane get` every second, so reading tokens is free.
**REWORK, three ways.** (a) **Token values truncate silently at exactly 80 characters** (measured:
81→80, 5000→80). Must be **address-keyed** — a demangled C++ symbol or a `~/.cache/bn/bndb/...`
selector overflows it, and prose is impossible. (b) **`--source` is not readable back** — `pane get` and
`api snapshot` both return a flat map with no attribution, and same-named writes clobber across sources.
(c) **The perf win is nil** — herdr calls are ~1.9 ms each; "idle cost halves" means saving ~2 ms/s. Lead
with correctness, not latency.
**The problem is also smaller than claimed:** the lens's own ask locator emits `-t <target>`, which *is*
a scope marker, and `ctx.rs:497,518-528` encodes that deliberately. The scrape is blind only before the
first ask, or once that marker scrolls past 400 lines. Real remaining value: **exactness** (vs harvesting
every ≥3-char identifier), and **detecting an agent working a different instance/target** — which nothing
checks today.

**L6. Golden-schema test in bn.** — *~1 day*
There is no schema versioning anywhere (§1). A pytest asserting checked-in key-set snapshots for the ~12
result `kind`s the lens deserializes is the only mechanism that turns a silent lens regression into a red
bn CI run.
**Drop the version-floor half.** It is not free: no call `Ctx::build` already makes carries
`plugin_version` (`target info` and `session list` both lack it), so it means an extra `bn doctor` spawn
on every build *and* every `^R` — and `bn doctor` pings each instance and sha256-hashes the install
package. Either add a version field to an existing envelope, or skip it.
**Constraint:** the fixture binary must be a small purpose-built ELF, never a dogfood target
(CLAUDE.md sanitization rule).

---

## 5. Explicit non-goals — measured dead ends

**Do not re-propose these without new measurements.** Recorded because each looks obviously correct.

### 5.1 Parallelizing `Ctx::build`'s reads — REFUTED
Measured **1.14×**, not the estimated 1.68× (1.618→1.417 s, 1.780→1.556 s, 1.644→1.440 s). Only
client-side CPython startup parallelizes; the bridge serializes on the GIL / BN core. Under a 6-way pool
`function list` inflated 0.842→1.576 s and `strings` 0.377→0.995 s.
**Actively harmful for pairing:** a writer must wait for all in-flight readers to drain, so 6-way
inflates the read-lock hold to ~1.6 s and delays the agent's rename/comment **longer** than the serial
build it replaces.

### 5.2 Fanning out `usage::report`'s decompiles — REFUTED
Serial **1.325 s** vs 6-way **1.941 s**. Individual calls inflate 0.21 → 1.90 s. Same root cause as 5.1.
The peek needs a spinner, not threads.

### 5.3 Speculative prefetch of the selected row — REFUTED
Two independent problems. It only covers `decompile` (223 ms) and leaves `local_list` (319 ms) on the
Enter path, so Enter goes 540 → 320 ms, not "instant". And an *un-landed* prefetch **actively slows the
Enter it was meant to help**: a decompile alone is 0.212 s; the same decompile with one background
decompile in flight is **0.344 s (+62%)**. It is also the only proposal that makes the lens a source of
unrequested bridge traffic.

### 5.4 Connection reuse and multi-op batching — REFUTED
connect+close is **95 µs** median against a 213 µs round-trip; a 7-op startup would save ~1.4 ms out of
663 ms. Both require changing `handle()`/`dispatch()`, and batching has no partial-failure envelope,
breaks the one-op-one-lock-class model, and breaks the single-`request_id` cancellation path.

### 5.5 Eliminating `--out` while staying on the CLI — REFUTED
Measured **675 ms with `--out` vs 651 ms on stdout** for an 875 KB payload — ~24 ms, in exchange for two
fragile coupled settings (`BN_SPILL_TOKENS` plus an explicit `--limit`) and reintroducing the exact
truncation N2 fixes. The temp-file dance disappears for free under L1.

### 5.6 A dedicated lens↔agent RPC channel (socket, FIFO, or sidecar state file) — REJECTED ON DESIGN
It duplicates a channel the lens already polls, and it forks the trust model: ask delivery is fail-closed
on herdr's pane+session identity **precisely because herdr is the authority on who is in which pane**. A
lens-owned channel would have to reinvent identity, and the likely outcome is a channel that fails *open*
where the ask channel fails closed. Rich payloads (taint results, findings, answers) belong in the live
bn instance as **comments and tags** — which the Marks view already reads and any `bn` command can read
too. Adding a listener to a UI thread where **no** `Command::output()` is time-bounded is also a new way
to wedge the TUI.

### 5.7 Follow-mode auto-navigation from herdr tokens — REJECTED ON DESIGN
`--source` is unreadable (L5b), so any process that can reach the herdr socket can publish `bn_goto` onto
the lens pane indistinguishably from the paired agent. An **invitation banner requiring a keypress** is
fine; auto-navigating on an unauthenticated token is not.

### 5.8 A `lens_boot` composite bn op — REJECTED ON DESIGN
Couples bn's client-neutral surface to one client, for a latency win the socket transport erases anyway
(7 direct round-trips ≈ 6 ms). The torn-`Ctx` risk it identifies is real — 7 independent read-lock
acquisitions with the agent free to commit between any two — but the fix is a generic "run these under
one lock" op, not a client-specific one. Take N2 and move on.

### 5.9 Token-typed decompile — NOT SHIPPABLE AS WRITTEN
Correctly sequenced last. Its stated wins do not survive: `Ctx::strings` is a `OnceCell` built on first
*use*, not at startup, so `bn strings` is never "paid twice at startup"; and none of the 7 `Ctx::build`
round-trips can be deleted, because `func_names`/`addr_by_name`/`data_names` feed the picker, exports,
imports, classes, marks, **and** `build_spans` for the MLIL/disasm/xrefs views the proposal explicitly
keeps on the legacy path.
Two unbudgeted blockers: `_decompile_text` silently degrades to wrapped HLIL when pseudo-C fails
(`il_format.py:859-880`), and `_pseudo_c_text` `lstrip()`s the header and prefixes an 8-hex gutter under
`--addresses` (`:840-846`), so token column offsets do not align with the text the lens renders.
**UNMEASURED and load-bearing:** nobody has checked whether BN types pseudo-C tokens usefully or just
emits `TextToken` for everything.

### 5.10 Version floor / attribution work inside `Ctx::build` — REJECTED
Both add unbudgeted subprocesses to the hottest path. The attribution proposal puts 2 `tag type create` +
`comment list` + `tag list` into `Ctx::build` — ~4 extra spawns × ~130 ms on startup **and** every `^R`,
~40% added startup latency, to populate a 12-row list. Do it lazily on first `t` press if wanted.
`bn tag type create` is also a **new write kind**, not on CLAUDE.md's enumerated write surface — it needs
an explicit decision, not a drive-by. A read-only fallback exists: convention on the `data` field of a
`Bookmarks` tag.

---

## 6. The transport decision (L1)

Two mutually exclusive forks. **Do not start either before the spike in §7.**

### Option A — direct AF_UNIX client in Rust
Speak the protocol in §1 from `bn.rs`. ~2–5 days.

**Blockers to budget (all understated in the original proposal):**
- **"Zero renderer porting on the hot path" is false.** `decompile`'s CLI renderer is
  `_resolution_note(value) + text + warnings` (`function.py:252-257`, `formatters.py:124-143`), and the
  viewer *does* open interior addresses (`viewer.rs:404`), so dropping it removes the
  `0x… is inside <fn>; showing the containing function` disclosure.
- **No timeout/cancel plan.** bn's client sets a per-request timeout and fires `cancel_request` on a
  separate connection when it trips (`transport.py:433-451`). A Rust client that skips this **orphans
  in-flight bridge work**.
- **`limit` is a raw param on the socket** with no `_effective_limit` layer — every paged op must pass
  `limit: null` deliberately.
- **`decompile --force-analysis` escalates read→write** (`bridge.py:1070-1073`). Do not assume decompile
  is read-locked.

### Option B — `bn serve`, an NDJSON daemon on the bn side
~80–90% of the win at **1.5–2.5 days**, strictly safer on compatibility, and it is **the only option that
also speeds up the agent's own `bn` calls** (a 40-command survey saves ~5 s).
Verified in its favor: `parse_args` on a *reused* parser is **0.14–0.18 ms** and worked cleanly across two
command paths.
**Unlisted blocker:** `SystemExit` — `BnArgumentParser.error()` → `parser.exit(2)`, and `--version` is an
`action="version"`. Every request must catch it and map `.code` to an rc.

### What is still unmeasured
No raw-socket timing exists for `decompile`, `xrefs`, or `taint` — only `doctor` (0.21 ms), `sections`
(0.64 ms), and `target_info` (**58.7 ms**). The "0.9 ms on the wire" framing is a **cheap-read number, not
a universal one**; `target_info` proves a single op really can cost ~59 ms bridge-side.

---

## 7. The spike — do this before committing to §6

**Byte-diff `decompile` over the raw socket against `bn decompile <fn>` stdout, for one real function on
a live instance.** Under an hour.

```
socket:  {"id":"probe","op":"decompile","params":{"identifier":"<fn>"},"target":null}  -> result.text
cli:     bn -i <id> -t <sel> decompile <fn>                                            -> stdout
diff
```

**Why this and not N2:** N2 is a 10-minute bug fix that de-risks nothing. This hour decides the largest,
riskiest item in the plan and picks between two multi-day forks.

**What it settles:** the socket port's entire cost estimate rests on "the hot path needs zero renderer
porting". Verification already dented that (§6 Option A). This diff measures exactly how much renderer
surface is really in play.

**Decision rule:**
- **Identical for an exact-name lookup, differing only by the resolution note on an interior address** →
  the hot path is confirmed cheap. Budget the note (~15 lines) plus the xrefs / `class show` /
  `types show` / `read` renderers, and take **Option A** (3–5 days).
- **Differs in any unexpected way** → the renderer surface is larger than modelled, the 3–5 day estimate
  is wrong, and **Option B** (1.5–2.5 days, byte-identical output by construction, and it also speeds up
  the agent) becomes the better bet.

**While the socket is open,** also time `decompile` and `xrefs` raw — the two ops whose real bridge cost
is currently unmeasured (§6).

---

## Appendix A — measurement method and raw data

**Environment:** CPython 3.14.2 via uv tool shim; 5 live bridge instances holding ~5.9 GB RSS combined;
warm cache (binaries and `.pyc` already hot — a genuinely cold first invocation is worse). 5 runs per
command, min and median reported. `perf_counter` numbers include ~2–5 ms of subprocess fork/exec from the
measuring harness, so treat them as a slight upper bound; `/usr/bin/time -f %e` agreed within 10 ms.

**Nothing was mutated.** Only `session list`, `doctor`, `sections`, `target info`, `--help`, `--version`,
and raw reads of `lock="read"` ops were issued. No load, close, save, refresh, or write op.

| what | command | min (ms) | median | note |
|---|---|---|---|---|
| interpreter floor | `python -c pass` | 16.6 | 17.1 | no bn code |
| + import bn.cli | `python -c "import bn.cli"` | 74.5 | 76.0 | ~58 ms on top of the interpreter |
| CLI, no socket | `bn --version` | **126.8** | 133.0 | the per-invocation floor for *any* bn call |
| CLI, full parser | `bn --help` | 129.3 | 131.2 | building the ~35-subcommand parser is not the expensive part; importing is |
| cheapest round-trip | `bn -i <id> doctor` | 134.9 | 136.8 | only ~5–8 ms above `--version` |
| small real read | `bn -i <id> sections` | **127.6** | 129.8 | 5.7 KB out; indistinguishable from `--version` |
| heavier read | `bn -i <id> target info --format json` | 179.7 | 185.8 | the extra ~55 ms is genuine bridge work |
| **raw socket** | `doctor` | **0.21** | 0.23 | 544-byte reply, `ok=true` |
| **raw socket** | `sections` | **0.64** | 0.87 | 5,688-byte reply — **the headline** |
| **raw socket** | `target_info` | **58.7** | 61.8 | one outlier at 126 ms; accounts exactly for the CLI gap (186 − 129 ≈ 57) |

**Derived:** ~99.3% of a `bn sections` call is Python process startup; ~0.7% is work.

**Not profiled:** which imports dominate the ~58 ms `bn.cli` import. That needs `-X importtime`.

---

## Appendix B — incidental findings

- **`/opt/bn/herdr` is a stale copy of bn-lens v1.5.0**, not herdr source. 12 `src/` files vs the current
  29. Delete or re-point it before it misleads someone.
- **This worktree has no `target/`.** The built binary exists only at `/opt/bn-tui/target/release/bn-lens`
  (1.70 MiB). Run `cargo build --release` in the worktree before `herdr plugin link` — local linking skips
  `[[build]]`.
- **`same_agent_session` fails open when `expected` is empty** (`herdr.rs:84`). If launch captured no
  session id, any later agent in that pane receives asks. Worth closing regardless of anything else here.
- Audit scratch scripts were left at `/tmp/claude-1005/sock_probe.py` and `/tmp/claude-1005/timecli.py`.
  `timecli.py` hardcodes an instance id — inspected, and it is a benign dogfood id revealing no target
  name or path, so nothing leaked. Delete when convenient.

---

## Appendix C — provenance

Produced by an 18-agent audit: 6 parallel deep-readers over both codebases, 1 empirical latency pass, 5
independent design angles (transport, data contract, agent pairing, capability surface, UX/perf), each
adversarially verified against source by a separate agent instructed to default to refuting, then
synthesized. 2.1 M subagent tokens, 662 tool calls, ~29 min wall.

Every REWORK and REFUTED note in §4 and §5 is a proposal that did **not** survive that verification pass
unchanged. Claims marked UNMEASURED are exactly that — flagged rather than estimated.
