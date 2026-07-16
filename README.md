# bn lens

**A headless Binary Ninja navigator TUI that pair-programs with your agent — as a [herdr](https://herdr.dev) plugin.**

When you're reverse-engineering a binary through the `bn` CLI with a coding agent, `bn lens` gives you
a fast, read-only view *beside* the agent's pane: filter to a function, read its highlighted
decompile, jump through callees and xrefs, peek at data — and, the point, **ask the agent about any
line or block** without leaving the binary.

It does not try to be Binary Ninja. It's the thing BN doesn't have: a keyboard-driven navigator for
when you're **headless** (no GUI), glued to the agent doing the work.

```
 bn lens   sockd · aarch64   ⟩ inst-a3f · 112 fns
 0x402760  decompile  handle_msg
 j/k · Tab sym · g goto · p peek · x xrefs · V select · ? ask · b back · q list
────────────────────────────────────────────────────────────────────────────────
   1▸│ int64_t handle_msg(struct ClientCtx* ctx, uint16_t* frame)
   2 │
   3 │ {  // Handler for msg id 1: copies attacker uint16 frame length into
    ↳│  msg_buf_alloc()+header. Framing caps len <= 0x9c4 …
   4 │     int64_t x0 = msg_buf_alloc();
```

## Install

Requires the [`bn`](https://binary.ninja) CLI bridge on your `PATH` (or `~/.local/bin`), a Rust
toolchain, and herdr ≥ 0.7.

```sh
git clone <this> /path/to/bn-lens
cd /path/to/bn-lens
cargo build --release          # herdr's [[build]] does this on GitHub-install; local link skips it
herdr plugin link .
```

Then bind a key in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "alt+d"
type = "plugin_action"
command = "bn.lens.open"
```

Press **`alt+d`** in any pane whose binary has a `bn` session open. A picker opens beside it.

## Keys

The header bar shows `bn lens · -i <instance> · -t <target> · <arch>` — copy the `-i`/`-t` straight
into a `bn` command. Keys live in the **footer**.

**Picker** — every function in **address order**, with a **`── recent ──`** subsection on top:
functions you opened here (`▸ you`), functions/addresses the agent referenced in its pane (`◆ agent`,
`★ both`), newest first and refreshed live. The recent group is a shortcut — every function still
appears in the by-address list. Non-function addresses show their **section + nearest symbol**
(`0x4152a0  .bss → __bss_start`) instead of `(addr)`. Keys: `j/k` `g`/`G` `^D`/`^U` move (skips the
delimiters) · `Enter` decompile · `x` xrefs · **`s` sections** · **`i` switch bn** · mouse wheel/click
· `q` quit. **`/` search**: type to filter, `↑`/`↓` pick among matches, `Enter` opens the
highlighted (top-ranked) hit directly, `Tab` keeps the filter but drops to the list (so `x`/`i` apply
to the narrowed set), `Esc` cancels.

**Switch (`i`)** — a ranger-style miller-columns view: **instances │ targets │ target info**. `j/k`
moves within a column, `h`/`l` (or `←`/`→`/`Tab`) between columns; the middle column lists the
highlighted instance's targets, the right column previews the highlighted target (arch, entry,
function counts, path). `Enter` re-points the lens at that instance+target; `Esc` cancels.

**Viewer**

| key | action |
|-----|--------|
| `j` / `k`, `^D`/`^U`, `G`, `Home` | move the line cursor (`N/total` shown in the header) |
| `/`, then `n` / `N` | **find in function** — jump between matches (`◆` marks them) |
| `Tab` / `Shift-Tab` | step through **interactive tokens** one at a time — *granular*, so two calls on one line (`f(g(x))`) are both reachable. Tokens are typed: **functions** (blue), **data globals** (cyan), **in-section addresses** (yellow), **string literals** (magenta), **locals** (gray). Constants/offsets (`0x120`) are inert. |
| `i` / `I` | **cycle the view** of this function: `i` forward, `I` back through **decompile → MLIL → disassembly**. Hotspots work in every view (branch targets in disasm/mlil are clickable addresses). |
| `g` / `Enter` | act on the selected token by kind: **goto** a function/code-address, **peek** a data global / data-address, or show a **local**'s type |
| `p` | **peek** — hex-dump the selected token's bytes (resolves internal symbols on-demand; a raw `0x…` peeks that section; a **string literal** resolves to its `.rodata` address; **pointers are symbolized** to names, so a function-pointer table reads as `+0x8→handle_…`). On a local, shows its type. |
| `x` | **xrefs** of the selected token — for a string literal, xrefs of its address (who else uses it) |
| `r` | **rename a local** — opens a dialog (name validated as an identifier); on confirm it renames via `bn local rename`, applied **live in the bn instance** and reflected in place (no re-decompile, no auto-save — the one place the lens mutates). Persisting to the on-disk `.bndb` is a deliberate `bn save`. Selecting a local also highlights all its occurrences. |
| click | select the token under the mouse |
| `s` | **sections** — scrollable map of address ranges, sizes, perms, semantics, and names, with a `w+x` summary line up top (also available from the picker) |
| `x` | **xrefs** of the symbol (Enter on a caller lands on the *use*) |
| `V` then `j`/`k` | **visual-select** a range of lines |
| `?` | **ask the agent** about this line (or the selected range) |
| `b` | back (navigation stack) |
| `q` | back to the picker |

**The pairing loop** — the whole point. Ask (`?`) → the header bar shows **where the ask goes and its
live status** (`◐ → wD:p1 claude working`), so you can both see you're wired to the right agent and
know when to glance at its pane. The message goes **only to the pane the lens was spawned from**, and
only if that pane still hosts the **same** agent it was spawned from (identity is checked by session
id). If there's no launching pane, or its agent changed or went away, `?` **fails closed** — it never
falls back to another agent (the header shows `⚠ ask off: no launching pane` and the dialog shows
`→ (no launching pane — cannot send)`). This matters because the payload carries real target names;
mis-delivery would leak them. When it does send (via `herdr pane run`), it's a single line carrying a
copy-pasteable locator + the highlighted code + your question:

```
[bn lens] -i <instance> -t <target> · <fn> @ <addr> · lines <lo>-<hi> · code: <code…> · [user] <question>
```

The `-i`/`-t` selector and `<fn> @ <addr>` anchor let the agent re-query the exact spot
(`bn -i … -t … decompile <fn>`) and pull as much surrounding context as it needs; the inlined code
lets it answer local questions in one shot. It stays on one line on purpose — `herdr pane run` treats
an embedded newline as a submit.

## Configuration (env)

| var | default | meaning |
|-----|---------|---------|
| `BN_LENS_INSTANCE` | — | force a specific `bn` instance (else auto-resolved) |
| `BN_LENS_SPLIT` | `right` | split direction of the picker (`right`/`down`) |
| `BN_LENS_BN_PATH` / `HERDR_BIN_PATH` | resolved | override the `bn` / `herdr` binaries |

**Instance resolution** is automatic: the `.bn-<id>` marker in the launching pane's cwd → the single
live instance → the newest-started live one, self-healing past a stale marker that points at the
wrong (functionless) instance.

## Design & internals

See [`DESIGN.md`](DESIGN.md). In short: Rust + `ratatui`/`crossterm`, ten small modules, a pure
unit-tested pseudo-C tokenizer, one-way data flow (picker → `Action`, viewer → `Exit`), read-only by
construction (never mutates the BNDB). `cargo test` covers the tokenizer.

## License

Personal tooling; use at your own risk.
