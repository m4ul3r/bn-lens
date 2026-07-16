//! Thin wrappers over the `herdr` CLI: read a pane, prompt the agent in a pane,
//! open the picker pane, and parse the plugin invocation context.

use serde::Deserialize;
use std::process::Command;

pub fn bin() -> String {
    crate::bn::resolve_bin("herdr", "HERDR_BIN_PATH", &["~/.local/bin/herdr"])
}

/// The context herdr injects for a plugin action (subset we use).
#[derive(Deserialize, Default)]
pub struct Context {
    #[serde(default)]
    pub focused_pane_id: Option<String>,
    #[serde(default)]
    pub focused_pane_cwd: Option<String>,
    #[serde(default)]
    pub workspace_cwd: Option<String>,
}

pub fn context() -> Context {
    std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Read recent scrollback of a pane (best-effort; empty on failure).
pub fn pane_read(herdr: &str, pane: &str, lines: usize) -> String {
    Command::new(herdr)
        .args([
            "pane",
            "read",
            pane,
            "--source",
            "recent-unwrapped",
            "--lines",
            &lines.to_string(),
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct PaneGet {
    result: PaneGetResult,
}
#[derive(Deserialize)]
struct PaneGetResult {
    pane: PaneInfo,
}
#[derive(Deserialize)]
struct PaneInfo {
    #[serde(default)]
    agent_status: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_session: Option<AgentSession>,
}
#[derive(Deserialize)]
struct AgentSession {
    #[serde(default)]
    value: Option<String>,
}

/// The detected agent hosted by a pane: its status, stable session id, and kind.
/// Identity (the `session` id) is what lets us confirm the `?` target is still
/// the *same* agent the lens was spawned from.
#[derive(Clone, Default)]
pub struct PaneAgent {
    pub status: String,
    pub session: String,
    pub agent: String,
}

/// The agent hosted by a pane, or None if the pane is gone or hosts no detected
/// agent (a plain shell). Never guesses — an empty pane id yields None.
pub fn pane_agent(herdr: &str, pane: &str) -> Option<PaneAgent> {
    if pane.is_empty() {
        return None;
    }
    let out = Command::new(herdr)
        .args(["pane", "get", pane])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())?;
    let p = serde_json::from_str::<PaneGet>(&out).ok()?.result.pane;
    p.agent.as_ref()?; // None => no detected agent
    Some(PaneAgent {
        status: p.agent_status.unwrap_or_default(),
        session: p.agent_session.and_then(|s| s.value).unwrap_or_default(),
        agent: p.agent.unwrap_or_default(),
    })
}

/// Send `text` to the agent in a pane as a prompt (herdr's documented path).
/// Returns true on success.
pub fn pane_run(herdr: &str, pane: &str, text: &str) -> bool {
    Command::new(herdr)
        .args(["pane", "run", pane, text])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Open the picker as a split beside `target_pane` (falls back to overlay when
/// no target pane is known). Returns the raw JSON response.
pub fn open_picker(herdr: &str, target_pane: &str, envs: &[(&str, &str)]) -> String {
    let mut c = Command::new(herdr);
    c.args(["plugin", "pane", "open", "--plugin", "bn.lens", "--entrypoint", "picker"]);
    if target_pane.is_empty() {
        c.args(["--placement", "overlay", "--focus"]);
    } else {
        c.args([
            "--placement",
            "split",
            "--target-pane",
            target_pane,
            "--direction",
            &std::env::var("BN_LENS_SPLIT").unwrap_or_else(|_| "right".into()),
            "--focus",
        ]);
    }
    for (k, v) in envs {
        c.args(["--env", &format!("{k}={v}")]);
    }
    c.output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}
