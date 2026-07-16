//! bn lens — a headless Binary Ninja navigator TUI that pair-programs with the
//! agent that launched it. Two modes:
//!   bn-lens launch   (herdr action) — open the picker beside the focused pane
//!   bn-lens picker   (herdr pane)   — run the TUI
mod app;
mod bn;
mod ctx;
mod decomp;
mod help;
mod herdr;
mod launch;
mod menu;
mod picker;
mod strings;
mod switch;
mod syntax;
mod theme;
mod ui;
mod viewer;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let code = match mode.as_str() {
        "launch" => launch::run(),
        _ => app::run(),
    };
    std::process::exit(code);
}
