//! The `launch` action: read the herdr invocation context and open the picker
//! as a split beside the focused pane.

use crate::herdr;

pub fn run() -> i32 {
    let ctx = herdr::context();
    let pane = ctx
        .focused_pane_id
        .or_else(|| std::env::var("HERDR_PANE_ID").ok())
        .unwrap_or_default();
    let cwd = ctx
        .focused_pane_cwd
        .or(ctx.workspace_cwd)
        .unwrap_or_default();
    let herdr_bin = herdr::bin();
    // Record the launching agent's session id so the lens can later confirm the
    // Ask target is still the *same* agent it was spawned from (not just any
    // agent that happens to occupy the pane).
    let session = herdr::pane_agent(&herdr_bin, &pane)
        .map(|a| a.session)
        .unwrap_or_default();
    // An unreadable id is not a soft failure: `same_agent_session` is fail-closed
    // on an empty expected id, so the lens comes up with asks disabled (the
    // header says so in red). Say why here too — from the picker alone a
    // mis-wired launch is indistinguishable from a pane whose agent changed.
    if session.is_empty() {
        eprintln!(
            "bn lens: could not read the launching agent's session id — \
             asks will be disabled for this lens"
        );
    }
    let out = herdr::open_picker(
        &herdr_bin,
        &pane,
        &[
            ("BN_LENS_PANE", &pane),
            ("BN_LENS_CWD", &cwd),
            ("BN_LENS_AGENT_SESSION", &session),
        ],
    );
    if out.contains("\"error\"") {
        eprintln!("bn lens: could not open picker: {out}");
        return 1;
    }
    0
}
