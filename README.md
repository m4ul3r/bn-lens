# bn lens

A **vim-inspired, headless Binary Ninja navigator** for your terminal that pair-programs with a coding
agent — a [herdr](https://herdr.dev) plugin.

It's built for reverse-engineering / vuln-research on a **remote box** (SSH'd in, no GUI): dig through
one — or several — `.bndb`s the way you'd move through vim (filter, goto, xref, peek, cycle
decompile / MLIL / disasm), and **loop your agent in on any line** while you both work the same
database. No Binary Ninja window; just a fast keyboard navigator beside the agent doing the work.

## Requires

- **Binary Ninja with a headless-capable license** (Commercial or Ultimate — headless isn't available
  on the Personal license). Everything the lens shows comes from headless BN analysis.
- **[`m4ul3r/bn`](https://github.com/m4ul3r/bn)** — the Binary Ninja CLI bridge. **Hard requirement:**
  the lens is built against it and shells out to it (which drives headless BN) for every operation.
  Install it and put `bn` on your `PATH` (or `~/.local/bin`).
- A **Rust** toolchain and **herdr ≥ 0.7**.

## Build & install

```sh
cd /path/to/bn-tui
cargo build --release            # → target/release/bn-lens
herdr plugin link /path/to/bn-tui   # local link needs the binary pre-built (skips [[build]])
```

Bind a key in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "alt+d"
type = "plugin_action"
command = "bn.lens.open"
```

## Use it in herdr

1. Start a `bn` session on your target(s) — one `.bndb` per binary:
   `bn session start /path/to/binary` (open several to work across binaries).
2. In any herdr pane, press **`alt+d`** — a picker opens *beside* your work, wired to that pane's
   agent and the resolved `bn` instance.
3. Filter to a function, `Enter` to read it, step through its hotspots, `a` to ask your agent about a
   line. Press **`i`** in the picker to switch between instances/targets when juggling multiple
   `.bndb`s.

### A preferred workflow

SSH into the box → `bn session start` your target(s) → in herdr, keep your agent (claude/codex) in one
pane and open **bn lens beside it** (`alt+d`).

- **Navigate** in the lens like vim: `/` to a function, `Enter`, then `Tab` through hotspots, `g` to
  follow calls, `x` for xrefs, `p` to peek data/strings, `i`/`I` to cross-check decompile ↔ MLIL ↔
  disasm.
- **Hand off to the agent** when something's worth a deeper look: `V` to select a range, `a` to send
  it. The lens delivers a copy-pasteable `-i/-t/fn@addr` locator + the code to the agent's pane; the
  agent re-queries via `bn` (xrefs, taint, types…) and you **follow along in the lens**.
- **Annotate as you learn:** `r` renames a local (live in the instance). Recover names, then keep
  moving.
- **Chase a cross-binary trail:** `i` (picker) re-points the lens at another `.bndb`; the agent and
  lens stay in sync per instance/target.

The picker's **`recent`** section keeps a live list of what you opened (`▸`) and what the agent
referenced in its pane (`◆`) — a shared map of where you both are.

## Keyboard shortcuts

Press **`?` anywhere** for the complete, scrollable shortcut guide. The status bars show only the
most useful keys for the current mode.

**Picker**

| key | action |
|-----|--------|
| `?` | open the global shortcut guide |
| `j`/`k` `g`/`G` `^D`/`^U` | move (skips the section delimiters) |
| `/` | search — type to filter, `↑`/`↓` pick, `Enter` opens the top hit, `Tab` keeps the filter, `Esc` cancels |
| `Enter` / `x` | decompile / xrefs the selected function |
| `s` | sections map (perms, ranges, names, `w+x` flag) |
| `i` | switch bn — ranger view over **instances │ targets │ info**; `Enter` re-points the lens |
| mouse | wheel scroll / click to select · `q` quit |

**Viewer**

| key | action |
|-----|--------|
| `?` | open the global shortcut guide |
| `j`/`k` `^D`/`^U` `G` | move the line cursor |
| `Tab` / `Shift-Tab` | step through **hotspots** — functions (blue), data (cyan), addresses (yellow), strings (magenta), locals (gray) |
| `g` / `Enter` | act on the hotspot: goto a function/code address, peek data, show a local's type |
| `p` | peek bytes (symbolizes pointers; strings resolve to their `.rodata` address) |
| `x` | xrefs of the hotspot (`Enter` on a caller lands on the *use*) |
| `r` | rename a local (live; persist to disk with `bn save`) |
| `i` / `I` | cycle view: **decompile → MLIL → disassembly** (forward / back) |
| `/` then `n`/`N` | find in function |
| `V` then `a` | visual-select a range, then **ask the agent** |
| `a` | ask the agent about the cursor line |
| `s` | sections · `b` back · `q` to the picker |

## Configuration (env)

| var | meaning |
|-----|---------|
| `BN_LENS_INSTANCE` | force a specific `bn` instance (else auto-resolved from the pane) |
| `BN_LENS_SPLIT` | picker split direction (`right` / `down`) |
| `BN_LENS_BN_PATH` / `HERDR_BIN_PATH` | override the `bn` / `herdr` binaries |

## Design & internals

See [`DESIGN.md`](DESIGN.md). Rust + `ratatui`/`crossterm`, focused modules, unit-tested token/hotspot
helpers, one-way data flow. Read-only except one deliberate mutation (local rename). `cargo test` runs
the suite.

## License

Personal tooling; use at your own risk.
