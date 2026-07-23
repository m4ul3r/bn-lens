//! Direct AF_UNIX client for `bn_agent_bridge`, replacing the per-call `bn` CLI spawn.
//!
//! A `bn` invocation costs ~127 ms of Python interpreter startup before any work
//! happens; the same read over this socket costs ~0.6 ms. See
//! `DESIGN_BN_INTERFACE.md` §2 for the measurements and §6 for why this path is
//! supported rather than a hack (the bridge validates at the op layer, not the CLI
//! layer, and its own source calls out "raw-socket / py exec caller").
//!
//! ## Protocol (verified against `/opt/bn/src/bn_agent_bridge/bridge.py`)
//!
//! - AF_UNIX SOCK_STREAM at `<cache>/bn/instances/<id>.sock`, mode 0600, SO_PEERCRED
//!   same-uid only. No handshake, no auth token, no version negotiation on the wire.
//! - Request: **one** JSON object plus `\n` — `{"id", "op", "params", "target"?}`.
//!   `bridge.py:714` reads it with a single `readline(MAX_REQUEST_BYTES)` (32 MiB).
//! - Response: one JSON object with **no trailing newline and no length prefix**
//!   (`_json_response`, `_shared.py:32`), so the reply is delimited by EOF only —
//!   read to EOF, never `read_line`.
//! - Envelope is always `{"ok": bool, "result": any, "error": string|null}`.
//! - **One request per connection.** `handle()` does a single readline then returns,
//!   so a second request on the same stream is silently dropped. Connection reuse was
//!   measured and rejected (`DESIGN_BN_INTERFACE.md` §5.4): connect+close is ~95 µs
//!   against a ~213 µs round trip.
//!
//! ## Two behaviors that are easy to get wrong
//!
//! 1. **Cancellation.** On timeout the bridge is still working. `bn`'s own client
//!    fires a `cancel_request` op on a *separate* connection (`transport.py:433-451`);
//!    skipping it orphans in-flight bridge work. [`Client::request`] does the same.
//! 2. **Paging.** There is no `_effective_limit` layer here — that lives in the CLI
//!    (`cli.py:1286-1296`). On the wire, an absent `limit` means *no limit*
//!    (`bridge.py:3153`), which is what we want, but it must be a deliberate choice
//!    per call rather than an accident.

use serde::Deserialize;
use serde_json::{Map, Value};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Matches bn's `DEFAULT_REQUEST_TIMEOUT` (`transport.py:40`). Generous because
/// legitimate reads on a large binary genuinely run long.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Timeout for the best-effort cancel we fire after a request times out
/// (`transport.py:47`). Deliberately tiny: a wedged bridge must not make the
/// cancel itself hang the UI.
const CANCEL_TIMEOUT: Duration = Duration::from_millis(250);

/// Liveness probe budget when enumerating instances. Short — this runs during
/// instance resolution, on the startup path.
const LIVENESS_TIMEOUT: Duration = Duration::from_millis(200);

/// Connect attempts before giving up, mirroring `transport.py:477` so a bridge
/// that is mid-accept isn't reported dead.
const CONNECT_RETRIES: u32 = 4;

/// The bridge name used for the legacy fixed (GUI-mode) registry pair.
const PLUGIN_NAME: &str = "bn_agent_bridge";

/// `{"ok", "result", "error"}` — the only response shape the bridge emits.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<String>,
}

/// One instance registry file (`<cache>/bn/instances/<id>.json`).
#[derive(Deserialize)]
struct RegistryJson {
    #[serde(default)]
    pid: i64,
    #[serde(default)]
    socket_path: String,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    plugin_version: String,
    #[serde(default)]
    started_at: String,
    #[serde(default)]
    binaries: Vec<String>,
}

/// A live bridge instance reachable over its socket.
#[derive(Clone, Debug)]
pub struct Client {
    /// Selector as `-i` would spell it (`"default"` for the legacy fixed pair).
    pub instance_id: String,
    pub socket_path: PathBuf,
    pub plugin_version: String,
    pub started_at: String,
    /// Binaries the instance has open. Empty means it is idle — the lens skips
    /// those when auto-resolving, matching the previous `session list` behavior.
    pub binaries: Vec<String>,
}

/// `<cache>/bn`, mirroring `bn/paths.py:105-121` exactly (including `BN_CACHE_DIR`
/// and `XDG_CACHE_HOME`) so the lens and bn never disagree about where instances
/// live. The Darwin/Windows arms are omitted: the lens is Linux-only.
fn cache_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("BN_CACHE_DIR") {
        if !dir.is_empty() {
            return Some(expand_home(&dir));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("bn"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".cache").join("bn"))
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn instances_dir() -> Option<PathBuf> {
    cache_home().map(|dir| dir.join("instances"))
}

/// Whether a socket accepts a connection right now. This is the liveness signal
/// bn itself uses (`transport.py:205-212`) — a registry whose pid is gone but whose
/// socket file lingers must not be reported live.
fn socket_is_live(path: &Path) -> bool {
    connect_with_timeout(path, LIVENESS_TIMEOUT).is_ok()
}

fn connect_with_timeout(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    // `UnixStream::connect` has no timeout parameter; for AF_UNIX the connect
    // either completes immediately or fails, so a plain connect plus per-op
    // read/write timeouts is equivalent in practice.
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

/// Parse one registry file into a [`Client`], or `None` when it is stale/malformed.
///
/// Deliberately does **not** unlink stale registries or orphaned sockets the way
/// `transport.py:189-203` does. Purging is bn's job; the lens is a read-mostly
/// navigator and must not delete files another tool owns — a concurrent `bn`
/// spawn racing our unlink is exactly the failure that GC lock exists to prevent.
fn load_instance(registry: &Path, fallback_id: &str) -> Option<Client> {
    let text = std::fs::read_to_string(registry).ok()?;
    let parsed: RegistryJson = serde_json::from_str(&text).ok()?;
    if parsed.socket_path.is_empty() || parsed.pid <= 0 {
        return None;
    }
    let socket_path = PathBuf::from(&parsed.socket_path);
    if !socket_path.exists() || !socket_is_live(&socket_path) {
        return None;
    }
    Some(Client {
        instance_id: parsed
            .instance_id
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| fallback_id.to_string()),
        socket_path,
        plugin_version: parsed.plugin_version,
        started_at: parsed.started_at,
        binaries: parsed.binaries,
    })
}

/// Every live instance, newest-registry-first is *not* guaranteed — callers that
/// care about recency sort on [`Client::started_at`].
///
/// Mirrors `transport.py:243-261`: the legacy fixed registry (GUI mode) first,
/// then per-instance registries in sorted order.
pub fn list_instances() -> Vec<Client> {
    let mut found = Vec::new();
    if let Some(cache) = cache_home() {
        let legacy = cache.join(format!("{PLUGIN_NAME}.json"));
        if legacy.exists() {
            if let Some(client) = load_instance(&legacy, "default") {
                found.push(client);
            }
        }
    }
    let Some(dir) = instances_dir() else {
        return found;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return found;
    };
    let mut registries: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    registries.sort();
    for registry in registries {
        let stem = registry
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(client) = load_instance(&registry, &stem) {
            found.push(client);
        }
    }
    found
}

/// The live instance with this selector, or `None`.
pub fn open(instance_id: &str) -> Option<Client> {
    list_instances()
        .into_iter()
        .find(|client| client.instance_id == instance_id)
}

impl Client {
    /// Send one op and return its `result` payload.
    ///
    /// `target` is the `-t` selector; `None` lets the bridge use its active target.
    /// Errors are already human-facing (the bridge emits plain strings such as
    /// `Function not found: foo. Did you mean: fopen`).
    pub fn request(&self, op: &str, params: Value, target: Option<&str>) -> Result<Value, String> {
        self.request_timeout(op, params, target, DEFAULT_TIMEOUT)
    }

    pub fn request_timeout(
        &self,
        op: &str,
        params: Value,
        target: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, String> {
        // A per-request id is what makes cancellation addressable. Uniqueness only
        // has to hold among this process's in-flight requests, so pid+counter is
        // sufficient and avoids pulling in a uuid dependency (the crate tree is
        // deliberately 4 deps wide).
        let request_id = next_request_id();

        let mut payload = Map::new();
        payload.insert("id".into(), Value::String(request_id.clone()));
        payload.insert("op".into(), Value::String(op.to_string()));
        payload.insert("params".into(), params);
        // Omit rather than send null: `bridge.py` reads `payload.get("target")`, and
        // an explicit null is indistinguishable from absent, but omitting keeps the
        // request byte-identical to what bn's own client sends (`transport.py:470`).
        if let Some(target) = target {
            payload.insert("target".into(), Value::String(target.to_string()));
        }
        let mut encoded = serde_json::to_vec(&Value::Object(payload))
            .map_err(|error| format!("could not encode {op} request: {error}"))?;
        encoded.push(b'\n');

        let raw = match self.round_trip(&encoded, timeout) {
            Ok(raw) => raw,
            Err(RoundTripError::TimedOut) => {
                // The bridge is still working on it. Tell it to stop, on its own
                // connection, before surfacing the failure.
                self.cancel(&request_id);
                return Err(format!(
                    "timed out after {}s waiting for bn instance {} (op '{op}')",
                    timeout.as_secs(),
                    self.instance_id
                ));
            }
            Err(RoundTripError::Io(message)) => {
                return Err(format!(
                    "could not reach bn instance {} at {}: {message}",
                    self.instance_id,
                    self.socket_path.display()
                ))
            }
        };

        if raw.is_empty() {
            // The connection was accepted but closed without a reply. Every handler
            // exception is caught and serialized, so an empty reply means the
            // *process* died mid-request — a native fault or an OOM kill during
            // analysis (`transport.py:158-186` documents the same diagnosis).
            return Err(format!(
                "bn instance {} closed the connection without replying to '{op}' \
                 (the bridge process most likely crashed or was OOM-killed)",
                self.instance_id
            ));
        }

        let envelope: Envelope = serde_json::from_slice(&raw).map_err(|error| {
            format!(
                "bn instance {} returned invalid JSON for '{op}': {error}",
                self.instance_id
            )
        })?;
        if envelope.ok {
            Ok(envelope.result)
        } else {
            Err(envelope
                .error
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| format!("bn '{op}' failed")))
        }
    }

    /// Connect, send, read to EOF. Retries only the transient connect errors bn
    /// itself retries (`transport.py:30-33`) — a refused or missing socket can mean
    /// a bridge mid-accept, whereas any other error is real.
    fn round_trip(&self, encoded: &[u8], timeout: Duration) -> Result<Vec<u8>, RoundTripError> {
        let mut last: Option<std::io::Error> = None;
        for attempt in 0..CONNECT_RETRIES {
            match connect_with_timeout(&self.socket_path, timeout) {
                Ok(mut stream) => return read_response(&mut stream, encoded),
                Err(error) => {
                    let transient = matches!(
                        error.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::NotFound
                    );
                    last = Some(error);
                    if !transient || attempt + 1 == CONNECT_RETRIES {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50 * u64::from(attempt + 1)));
                }
            }
        }
        Err(RoundTripError::Io(
            last.map(|error| error.to_string())
                .unwrap_or_else(|| "connect failed".into()),
        ))
    }

    /// Best-effort `cancel_request` on a fresh connection. Failure is ignored on
    /// purpose: we are already reporting a timeout, and a second error would only
    /// obscure the first.
    fn cancel(&self, request_id: &str) {
        let payload = serde_json::json!({
            "id": next_request_id(),
            "op": "cancel_request",
            "params": {"request_id": request_id},
        });
        let Ok(mut encoded) = serde_json::to_vec(&payload) else {
            return;
        };
        encoded.push(b'\n');
        if let Ok(mut stream) = connect_with_timeout(&self.socket_path, CANCEL_TIMEOUT) {
            let _ = read_response(&mut stream, &encoded);
        }
    }
}

enum RoundTripError {
    TimedOut,
    Io(String),
}

/// Write the request, half-close so the bridge's `readline` returns, then read
/// until EOF — the reply carries no terminator of its own.
fn read_response(stream: &mut UnixStream, encoded: &[u8]) -> Result<Vec<u8>, RoundTripError> {
    stream.write_all(encoded).map_err(classify)?;
    stream.flush().map_err(classify)?;
    // Not strictly required (the trailing newline already ends the bridge's
    // readline) but it matches bn's client and makes a missing newline non-fatal.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(classify)?;
    Ok(raw)
}

fn classify(error: std::io::Error) -> RoundTripError {
    match error.kind() {
        // A read/write timeout surfaces as WouldBlock or TimedOut depending on
        // whether the socket had been set non-blocking; treat both as the timeout
        // they are, so the caller fires a cancel instead of reporting a bare IO error.
        ErrorKind::WouldBlock | ErrorKind::TimedOut => RoundTripError::TimedOut,
        _ => RoundTripError::Io(error.to_string()),
    }
}

fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("bn-lens-{}-{seq}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_distinguishes_ok_from_error() {
        let ok: Envelope = serde_json::from_str(r#"{"ok":true,"result":{"a":1},"error":null}"#)
            .expect("ok envelope");
        assert!(ok.ok);
        assert_eq!(ok.result["a"], 1);

        let err: Envelope =
            serde_json::from_str(r#"{"error":"Unknown operation: nope","ok":false,"result":null}"#)
                .expect("error envelope");
        assert!(!err.ok);
        assert_eq!(err.error.as_deref(), Some("Unknown operation: nope"));
    }

    #[test]
    fn a_missing_ok_field_is_not_success() {
        // Fail closed: a response shape we don't recognize must never read as ok.
        let envelope: Envelope = serde_json::from_str(r#"{"result":{}}"#).expect("partial");
        assert!(!envelope.ok);
    }

    #[test]
    fn registry_without_a_socket_path_is_rejected() {
        let parsed: RegistryJson =
            serde_json::from_str(r#"{"pid":123,"instance_id":"x"}"#).expect("registry");
        assert!(parsed.socket_path.is_empty());
    }

    #[test]
    fn registry_parses_the_fields_instance_resolution_needs() {
        let parsed: RegistryJson = serde_json::from_str(
            r#"{"pid":4242,"socket_path":"/tmp/x.sock","instance_id":"lens",
                "plugin_version":"0.20.0","started_at":"2026-01-01T00:00:00+00:00",
                "binaries":["/tmp/a.bin"]}"#,
        )
        .expect("registry");
        assert_eq!(parsed.pid, 4242);
        assert_eq!(parsed.instance_id.as_deref(), Some("lens"));
        assert_eq!(parsed.plugin_version, "0.20.0");
        assert_eq!(parsed.binaries.len(), 1);
    }

    #[test]
    fn request_ids_are_unique_within_a_process() {
        let a = next_request_id();
        let b = next_request_id();
        assert_ne!(a, b, "cancellation addresses a request by id");
        assert!(a.starts_with("bn-lens-"));
    }

    #[test]
    fn cache_home_honors_bn_cache_dir_over_xdg() {
        // Both env vars are process-global; set and restore around the assertion.
        let prev_bn = std::env::var("BN_CACHE_DIR").ok();
        std::env::set_var("BN_CACHE_DIR", "/tmp/bn-lens-test-cache");
        assert_eq!(
            cache_home().expect("cache home"),
            PathBuf::from("/tmp/bn-lens-test-cache")
        );
        match prev_bn {
            Some(value) => std::env::set_var("BN_CACHE_DIR", value),
            None => std::env::remove_var("BN_CACHE_DIR"),
        }
    }
}
