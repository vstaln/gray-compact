//! gray-compact — pi's "summarize EVERYTHING" compaction as a gray sidecar.
//!
//! Port of pi's `custom-compaction.ts` + `trigger-compact.ts`. On every
//! `context/build` the sidecar serializes the outbound message list; when it
//! exceeds `GRAY_COMPACT_CHARS` (default 120000) — or a `/compact` pass was
//! armed — it asks `host/run` (a `gray -p` turn, capability `host.turn`) to
//! summarize the oldest 70% and replies `{messages}` = one user message with
//! "## Earlier context (auto-compacted)\n<summary>" plus the newest 30%
//! verbatim. Summaries are cached per session in `~/.gray/compact/<sid>.json`
//! keyed by a hash of the summarized prefix, so the repeated `context/build`
//! calls inside one turn reuse the same summary instead of re-summarizing.
//!
//! `/compact` arms a pass for the next request (and says what it would do),
//! `/compact off|on|status` toggles and reports.
//!
//! `host/run` failures degrade gracefully: the sidecar replies `{}` (keep the
//! list) rather than ever blocking a turn.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// How long we wait for a `host/run` reply. The host caps the child `gray -p`
/// at 28s and the outer `command/run`/`context/build` deadline is 30s, so the
/// sidecar gives up just under the wire.
const RUN_TTL: Duration = Duration::from_secs(29);

const DEFAULT_THRESHOLD_CHARS: usize = 120_000;
/// Oldest 70% summarized; newest 30% of messages pass through verbatim.
const KEEP_NUM: usize = 3;
const KEEP_DEN: usize = 10;
/// pi truncates tool results at 2000 chars inside serialized summaries.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

fn manifest() -> Value {
    json!({
        "name": "compact",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/compact"],
        "hooks": ["context/build"],
        "capabilities": ["host.turn"],
    })
}

// --- host link --------------------------------------------------------------

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

/// Shared handles the main loop uses to call back into the host. `None` in
/// tests exercises the degraded path.
struct HostLink {
    out: Arc<Mutex<std::io::Stdout>>,
    pending: Pending,
    counter: Arc<Mutex<u64>>,
}

fn next_id(counter: &Arc<Mutex<u64>>) -> String {
    let mut n = counter.lock().expect("id counter");
    *n += 1;
    format!("q{n}")
}

/// `host/run` one prompt, returning the reply text. Every failure — no grant,
/// overload, timeout, malformed reply — is an `Err`, never a panic.
fn host_run(link: Option<&HostLink>, prompt: &str) -> Result<String, String> {
    let Some(link) = link else {
        return Err("host/run unavailable".into());
    };
    let id = next_id(&link.counter);
    let (tx, rx) = mpsc::channel();
    link.pending
        .lock()
        .expect("pending")
        .insert(id.clone(), tx);
    let req = json!({"id": id, "method": "host/run", "params": {"prompt": prompt}});
    {
        let mut o = link.out.lock().expect("stdout");
        if writeln!(o, "{req}").and_then(|_| o.flush()).is_err() {
            link.pending.lock().expect("pending").remove(&id);
            return Err("host/run: wire write failed".into());
        }
    }
    let reply = match rx.recv_timeout(RUN_TTL) {
        Ok(v) => v,
        Err(_) => {
            link.pending.lock().expect("pending").remove(&id);
            return Err("host/run did not answer in time".into());
        }
    };
    if let Some(text) = reply.pointer("/result/text").and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return Ok(text.to_string());
    }
    if let Some(e) = reply.pointer("/result/error").and_then(Value::as_str) {
        let hint = reply
            .pointer("/result/hint")
            .and_then(Value::as_str)
            .map(|h| format!(" ({h})"))
            .unwrap_or_default();
        return Err(format!("host/run: {e}{hint}"));
    }
    if let Some(e) = reply.pointer("/error/message").and_then(Value::as_str) {
        return Err(format!("host/run: {e}"));
    }
    Err("host/run: empty reply".into())
}

// --- conversation serialization (port of pi's serializeConversation) --------

/// `content` may be a string or gray's `[{type:...}]` block array; returns the
/// concatenated plain text ("" when absent or no text blocks).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let n = text.chars().count();
    if n <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}\n\n[... {} more characters truncated]", n - max_chars)
}

/// Serialize `[Message]` values to pi's `[Role]: ...` text form. Blocks are
/// gray's snake_case tags; the pi camelCase names are accepted too so the
/// serializer is honest about either serde shape.
fn serialize_messages(messages: &[Value]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        let empty = Vec::new();
        let blocks = content.as_array().unwrap_or(&empty);
        let text = content_text(&content);
        let label = match role {
            "user" => "User",
            "assistant" => "Assistant",
            "system" => "System",
            other if !other.is_empty() => other,
            _ => continue,
        };
        if role == "assistant" {
            let thinking: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("thinking"))
                .filter_map(|b| {
                    b.get("text")
                        .or_else(|| b.get("thinking"))
                        .and_then(Value::as_str)
                })
                .collect();
            if !thinking.is_empty() {
                parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
            }
            if !text.is_empty() {
                parts.push(format!("[Assistant]: {text}"));
            }
            let calls: Vec<String> = blocks
                .iter()
                .filter(|b| {
                    matches!(
                        b.get("type").and_then(Value::as_str),
                        Some("tool_use") | Some("toolCall")
                    )
                })
                .filter_map(|b| {
                    let name = b.get("name").and_then(Value::as_str)?;
                    let args = b
                        .get("args")
                        .or_else(|| b.get("arguments"))
                        .and_then(Value::as_object);
                    let args_str = args
                        .map(|o| {
                            o.iter()
                                .map(|(k, v)| format!("{k}={v}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    Some(format!("{name}({args_str})"))
                })
                .collect();
            if !calls.is_empty() {
                parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
            }
        } else if !text.is_empty() {
            parts.push(format!("[{label}]: {text}"));
        }
        // Tool results ride inside whatever message carries them (a user
        // message in gray, a toolResult message in pi).
        for b in blocks {
            if matches!(
                b.get("type").and_then(Value::as_str),
                Some("tool_result") | Some("toolResult")
            ) && let Some(c) = b.get("content")
            {
                let t = match c {
                    Value::String(s) => s.clone(),
                    other => content_text(other),
                };
                if !t.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&t, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }
    parts.join("\n\n")
}

/// pi custom-compaction's summary prompt, verbatim except the spec's added
/// "file paths / exact errors verbatim" clause.
fn summary_prompt(conversation_text: &str) -> String {
    format!(
        "You are a conversation summarizer. Create a comprehensive summary of this conversation that captures:\n\n\
         1. The main goals and objectives discussed\n\
         2. Key decisions made and their rationale\n\
         3. Important code changes, file modifications, or technical details\n\
         4. Current state of any ongoing work\n\
         5. Any blockers, issues, or open questions\n\
         6. Next steps that were planned or suggested\n\n\
         Be thorough but concise. The summary will replace the ENTIRE conversation history, so include all \
         information needed to continue the work effectively. Preserve file paths and exact error messages verbatim.\n\n\
         Format the summary as structured markdown with clear sections.\n\n\
         <conversation>\n{conversation_text}\n</conversation>"
    )
}

// --- per-session cache ------------------------------------------------------

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("compact")
}

fn sid_key(sid: &str) -> String {
    let k: String = sid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if k.is_empty() { "default".into() } else { k }
}

fn cache_path(sid: &str) -> PathBuf {
    state_dir().join(format!("{}.json", sid_key(sid)))
}

fn load_cache(sid: &str) -> Value {
    std::fs::read_to_string(cache_path(sid))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_cache(sid: &str, cache: &Value) {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(cache_path(sid), cache.to_string());
}

/// FNV-1a over the serialized prefix: cheap, no extra deps, stable enough for
/// "did the summarized span change".
fn prefix_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn threshold_chars() -> usize {
    std::env::var("GRAY_COMPACT_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_THRESHOLD_CHARS)
}

fn enabled() -> bool {
    !state_dir().join("disabled").exists()
}

/// `/compact` arms this; the next `context/build` compacts even under the
/// threshold, then clears it.
fn take_force() -> bool {
    let f = state_dir().join("force");
    if f.exists() {
        let _ = std::fs::remove_file(&f);
        true
    } else {
        false
    }
}

// --- context/build ----------------------------------------------------------

/// Build the compaction reply, or `None` to keep the outbound list unchanged.
fn compact_reply(messages: &[Value], sid: &str, link: Option<&HostLink>) -> Option<Value> {
    if !enabled() || messages.len() < 4 {
        return None;
    }
    let total_chars = serialize_messages(messages).chars().count();
    let forced = take_force();
    if !forced && total_chars <= threshold_chars() {
        return None;
    }
    let keep = (messages.len() * KEEP_NUM / KEEP_DEN).max(1);
    let prefix_len = messages.len() - keep;
    let prefix_text = serialize_messages(&messages[..prefix_len]);
    let hash = prefix_hash(&prefix_text);

    let mut cache = load_cache(sid);
    let summary = if cache.get("prefix_hash").and_then(Value::as_str) == Some(hash.as_str()) {
        cache.get("summary").and_then(Value::as_str).map(str::to_string)
    } else {
        match host_run(link, &summary_prompt(&prefix_text)) {
            Ok(s) => Some(s),
            // Fail open: a broken summarizer must never eat a request.
            Err(_) => None,
        }
    };
    let Some(summary) = summary else {
        cache["chars"] = json!(total_chars);
        cache["messages"] = json!(messages.len());
        save_cache(sid, &cache);
        return None;
    };
    cache["prefix_hash"] = json!(hash);
    cache["summary"] = json!(summary);
    cache["chars"] = json!(total_chars);
    cache["messages"] = json!(messages.len());
    save_cache(sid, &cache);

    let head = json!({
        "role": "user",
        "content": [{
            "type": "text",
            "text": format!("## Earlier context (auto-compacted)\n\n{summary}"),
        }],
    });
    let mut out = vec![head];
    out.extend(messages[prefix_len..].iter().cloned());
    Some(json!({ "messages": out }))
}

// --- /compact ---------------------------------------------------------------

fn run_command(argv: &[&str], sid: &str) -> String {
    match argv.first().copied() {
        Some("off") => {
            let dir = state_dir();
            match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join("disabled"), b""))
            {
                Ok(()) => "auto-compaction off".into(),
                Err(e) => format!("couldn't write state: {e}"),
            }
        }
        Some("on") => {
            let r = std::fs::remove_file(state_dir().join("disabled")).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(e)
                }
            });
            match r {
                Ok(()) => "auto-compaction on".into(),
                Err(e) => format!("couldn't flip state: {e}"),
            }
        }
        Some("status") => {
            let cache = load_cache(sid);
            let mut s = format!(
                "compact {} — {} · threshold {} chars (GRAY_COMPACT_CHARS)",
                env!("CARGO_PKG_VERSION"),
                if enabled() { "on" } else { "off" },
                threshold_chars(),
            );
            if let (Some(c), Some(m)) = (
                cache.get("chars").and_then(Value::as_u64),
                cache.get("messages").and_then(Value::as_u64),
            ) {
                s += &format!(" · last seen context {c} chars / {m} messages");
            }
            if cache.get("summary").and_then(Value::as_str).is_some() {
                s += " · cached summary ready";
            }
            s
        }
        Some(other) => format!("unknown subcommand {other:?} — /compact · /compact off|on|status"),
        None => {
            let dir = state_dir();
            let armed = std::fs::create_dir_all(&dir)
                .and_then(|_| std::fs::write(dir.join("force"), b""))
                .is_ok();
            if !armed {
                return "couldn't arm compaction (state dir unwritable)".into();
            }
            let cache = load_cache(sid);
            let what = match (
                cache.get("messages").and_then(Value::as_u64),
                cache.get("chars").and_then(Value::as_u64),
            ) {
                (Some(m), Some(c)) => format!(
                    "next request will replace the oldest 70% of {m} messages ({c} chars) with a summary, keeping the newest 30%"
                ),
                _ => "next request will summarize the oldest 70% of the context into one summary message".to_string(),
            };
            format!("compaction armed — {what}")
        }
    }
}

// --- wire loop ----------------------------------------------------------------

fn handle(req: &Value, link: Option<&HostLink>) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(),
        "context/build" => {
            let empty = Vec::new();
            let messages = params
                .get("messages")
                .and_then(Value::as_array)
                .unwrap_or(&empty);
            let sid = params
                .pointer("/session/id")
                .and_then(Value::as_str)
                .unwrap_or("");
            compact_reply(messages, sid, link).unwrap_or_else(|| json!({}))
        }
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let sid = params
                .pointer("/session/id")
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({ "text": run_command(&argv, sid) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return;
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(Mutex::new(0u64));

    // Reader thread: `host/run` replies (string id, no method) route to
    // pending waiters; numeric-id requests queue for the main loop.
    let (work_tx, work_rx) = mpsc::channel::<Value>();
    let reader_pending = pending.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(id) = v.get("id").and_then(|i| i.as_str())
                && v.get("method").is_none()
                && let Some(tx) = reader_pending.lock().expect("pending").remove(id)
            {
                let _ = tx.send(v);
                continue;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    for req in work_rx {
        let link = HostLink {
            out: stdout.clone(),
            pending: pending.clone(),
            counter: counter.clone(),
        };
        let (reply, exit) = handle(&req, Some(&link));
        if let Some(reply) = reply {
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{reply}");
            let _ = o.flush();
        }
        if exit {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        handle(&json!({ "id": 1, "method": method, "params": params }), None)
            .0
            .unwrap()
    }

    #[test]
    fn manifest_claims_context_build_and_host_turn() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "compact");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["context/build"]));
        assert_eq!(m["commands"], json!(["/compact"]));
        assert_eq!(m["capabilities"], json!(["host.turn"]));
    }

    fn msgs(n: usize, chars: usize) -> Vec<Value> {
        (0..n)
            .map(|i| {
                json!({
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "content": [{"type": "text", "text": "x".repeat(chars)}],
                })
            })
            .collect()
    }

    #[test]
    fn small_context_is_kept() {
        let r = call(
            "context/build",
            json!({"messages": msgs(6, 10), "session": {"id": "t1", "cwd": "/tmp"}}),
        );
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn huge_context_fails_open_without_host() {
        // Over threshold but no host link (or ungranted): keep the list.
        let r = call(
            "context/build",
            json!({"messages": msgs(6, 50_000), "session": {"id": "t2", "cwd": "/tmp"}}),
        );
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn serialization_matches_pi_shape() {
        let m = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "text": "hmm"},
                {"type": "text", "text": "doing it"},
                {"type": "tool_use", "id": "1", "name": "read", "args": {"path": "a.rs"}},
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "id": "1", "content": "file body", "is_error": false},
            ]}),
        ];
        let s = serialize_messages(&m);
        assert!(s.contains("[User]: hi"));
        assert!(s.contains("[Assistant thinking]: hmm"));
        assert!(s.contains("[Assistant]: doing it"));
        assert!(s.contains("[Assistant tool calls]: read(path=\"a.rs\")"));
        assert!(s.contains("[Tool result]: file body"));
    }

    #[test]
    fn tool_results_are_truncated() {
        let long = truncate_for_summary(&"y".repeat(3000), TOOL_RESULT_MAX_CHARS);
        assert!(long.contains("more characters truncated"));
        assert!(long.len() < 2200);
    }

    #[test]
    fn command_status_and_toggles() {
        let r = call(
            "command/run",
            json!({"name": "/compact", "argv": ["status"], "session": {"id": "t3"}}),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("threshold"));
    }

    #[test]
    fn bare_compact_arms_a_pass() {
        let r = call(
            "command/run",
            json!({"name": "/compact", "argv": [], "session": {"id": "t4"}}),
        );
        assert!(r["result"]["text"].as_str().unwrap().contains("armed"));
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle(&json!({ "id": 2, "method": "plugin/shutdown" }), None);
        assert!(reply.is_some() && exit);
        let (reply, exit) = handle(&json!({ "method": "plugin/shutdown" }), None);
        assert!(reply.is_none() && exit);
    }
}
