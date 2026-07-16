# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

`bn lens` is a headless Binary Ninja navigator TUI (Rust + ratatui/crossterm), packaged as a herdr plugin. It shells out to two external CLIs for everything: `bn` (the Binary Ninja bridge from `/opt/bn` — hard requirement, drives headless BN) and `herdr` (pane read, agent prompting, split management). There is no direct BN API usage in this repo.

## Build & Test

```bash
cargo build --release            # → target/release/bn-lens
cargo test                       # unit tests (pure logic: tokenizer, hotspots)
cargo test <name>                # single test by substring match
herdr plugin link /opt/bn-tui    # local link — requires the release binary pre-built (local linking skips [[build]])
```

The binary has two entry modes (`src/main.rs`): `bn-lens launch` (herdr action: reads pane context, opens the picker split) and `bn-lens picker` (the pane itself). The TUI is dogfooded in a herdr pane, not integration-tested; only pure helpers have unit tests.

## Architecture

One-way data flow, no global mutable state:

- `launch.rs` reads herdr context (pane, agent session) and spawns the picker split. Launch-time facts are passed via env (`BN_LENS_PANE`, `BN_LENS_AGENT_SESSION`, `BN_LENS_INSTANCE`, …).
- `ctx.rs` builds `Ctx` **once** (instance resolution, function list, data symbols, address/section maps, lazy strings) and shares it by reference.
- `app.rs` owns the terminal + event loop and the state machine. An `AppView` enum selects the active list — **Symbols** (`picker.rs`, functions + data) or **Strings** (`strings.rs`, built lazily) — both returning an `Action` (`OpenDecompile`/`OpenXrefs`/`Switch`/`Quit`); the viewer returns `Exit::{Stay,Back,Reload}`. The ` bn lens ` title is a clickable button (`menu.rs`) that switches view and reaches global actions. A `^R`/menu refresh rebuilds `Ctx` on a worker thread (non-blocking; a full-width bottom banner counts up while input is paused).
- `bn.rs` / `herdr.rs` are the only places that shell out. Big `bn` reads use `--out <file>` to dodge bn's stdout spill envelope.
- `syntax.rs` is a pure pseudo-C tokenizer producing `(text, kind)` runs; `viewer/hotspots.rs` runs a second pass (`build_spans`) promoting tokens into typed hotspots (Func/Data/Addr/Local/Str) using `Ctx` maps. Both are pure and unit-tested — new tokenizer/hotspot behavior needs tests.
- `viewer.rs` is the state model + load lifecycle; behavior is split across `viewer/actions.rs` (goto/peek/xrefs/rename), `viewer/input.rs` (key/mouse modes, search, view cycling, agent asks), `viewer/render.rs` (wrapped rendering, modals), `viewer/stack.rs` (stack-frame inspector, consumes `bn local list --format json`).
- `switch.rs` is the picker-only instance/target switcher; `help.rs` the global `?` overlay; `theme.rs` maps token kinds to colors (pseudo-C palette for decompile, muted `asm_style` for MLIL/disasm).

Views (decompile/MLIL/disasm/xrefs) are one `View` enum; hotspots build over all of them.

## Invariants to Preserve

- **Narrow write surface, all live-in-instance.** The write paths are rename (`r` — local or function), comment (`;`), bookmark/tag (`t`), and **type declaration** (Types view, `n` → `bn types declare`), all via `bn` and all in-memory on the live instance. None call `bn save` — persistence to the on-disk `.bndb` is deliberately deferred (see TODO.md). A local rename retexts tokens in place (`apply_local_rename`, no re-decompile) for latency; a function rename returns `Exit::Reload` so the app rebuilds ctx (name maps/callsites must update). Type declaration adds *whole* user types from a C declaration (validated with `--preview` before commit); **editing fields of existing structs and graph editing remain explicit non-goals** — the Types view reads existing layouts (`bn types show`) but only *adds* new types.
- **Ask delivery is fail-closed.** Agent asks go only to the launching pane (`BN_LENS_PANE`) and only if it still hosts the same agent (verified via `BN_LENS_AGENT_SESSION`). Never guess a recipient — a mis-delivered ask leaks real target names. The bn *instance*, by contrast, may auto-resolve (env → cwd `.bn-<id>` marker → single live → newest live, self-healing past stale markers).
- **Ask messages are a single line** — an embedded newline is a submit to `herdr pane run`.
- Render through ratatui's `Buffer` (clips instead of panicking); the Rust rewrite exists specifically to kill the curses bottom-right-cell crash class.

## Issues, PRs & Commits — Sanitize Test Data

This tool is dogfooded against real binaries (firmware, proprietary apps), same policy as `/opt/bn`. **Never disclose data from those targets in anything shared or committed** — GitHub issues, PR descriptions, commit messages, review notes, screenshots, or checked-in fixtures. Treat as sensitive: binary/target names, instance IDs, subsystem or product names, paths that reveal them, real function/symbol names, concrete addresses, and decompiled output lifted verbatim from a target.

Instead, **reproduce bugs or demonstrate fixes with realistic mock data that stands on its own.** Invent plausible function names, addresses, and structures that exhibit the same behavior, and keep them internally consistent so the example reads like a real session, understandable without access to the original binary.
