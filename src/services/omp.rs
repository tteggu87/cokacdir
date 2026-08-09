use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::LazyLock;
use std::time::Duration;

use crate::services::claude::{
    create_private_temp_file, debug_log_to, CancelToken, PrivateTempFile, StreamMessage,
};

/// Cached path to the omp binary.
static OMP_PATH: LazyLock<Option<String>> = LazyLock::new(resolve_omp_path);

/// Resolve the path to the omp binary.
/// First tries `which omp`, then falls back to `bash -lc "which omp"`.
#[cfg(unix)]
fn resolve_omp_path() -> Option<String> {
    if let Ok(val) = std::env::var("COKAC_OMP_PATH") {
        if !val.is_empty() && omp_path_is_runnable(&val) {
            return Some(val);
        }
    }

    if let Ok(output) = Command::new("which").arg("omp").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && omp_path_is_runnable(&path) {
                return Some(path);
            }
        }
    }

    if let Ok(output) = Command::new("bash").args(["-lc", "which omp"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && omp_path_is_runnable(&path) {
                return Some(path);
            }
        }
    }

    None
}

#[cfg(windows)]
fn resolve_omp_path() -> Option<String> {
    if let Ok(val) = std::env::var("COKAC_OMP_PATH") {
        if !val.is_empty() && omp_path_is_runnable(&val) {
            return Some(val);
        }
    }

    if let Some(path) = crate::services::claude::search_path_wide("omp", Some(".cmd")) {
        return Some(path);
    }
    if let Some(path) = crate::services::claude::search_path_wide("omp", Some(".exe")) {
        return Some(path);
    }
    None
}

#[cfg(not(any(unix, windows)))]
fn resolve_omp_path() -> Option<String> {
    std::env::var("COKAC_OMP_PATH")
        .ok()
        .filter(|path| !path.is_empty() && omp_path_is_runnable(path))
}

fn omp_path_is_runnable(path: &str) -> bool {
    let path = Path::new(path);
    if !path.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        matches!(ext.as_str(), "cmd" | "exe" | "bat" | "com")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        path.is_file()
    }
}

fn get_omp_path() -> Option<&'static str> {
    OMP_PATH.as_deref()
}
/// Check if the omp CLI is available.
pub fn is_omp_available() -> bool {
    get_omp_path().is_some()
}

/// Check if a model string refers to the omp backend.
pub fn is_omp_model(model: Option<&str>) -> bool {
    model
        .map(|model| model == "omp" || model.starts_with("omp:"))
        .unwrap_or(false)
}

/// Strip the `omp:` prefix and return the actual model name.
/// Returns None if the input is just `omp` (use the CLI default).
/// Also strips a display-name suffix (` — Description`) if present.
pub fn strip_omp_prefix(model: &str) -> Option<&str> {
    model
        .strip_prefix("omp:")
        .filter(|model| !model.is_empty())
        .map(|model| model.split(" \u{2014} ").next().unwrap_or(model).trim())
}

fn omp_debug_log(msg: &str) {
    debug_log_to("omp.log", msg);
}

#[derive(Debug, PartialEq)]
enum OmpEvent {
    Session(String),
    TextDelta(String),
    ToolStart(String, String),
    ToolResult { content: String, is_error: bool },
    FlushMarker,
    Other,
}

fn omp_result_text(json: &Value) -> String {
    json.get("result")
        .and_then(|result| result.get("content"))
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn classify_omp_json_line(line: &str) -> OmpEvent {
    let Ok(json) = serde_json::from_str::<Value>(line) else {
        return OmpEvent::Other;
    };
    let Some(event_type) = json.get("type").and_then(Value::as_str) else {
        return OmpEvent::Other;
    };

    match event_type {
        "session" => json
            .get("id")
            .and_then(Value::as_str)
            .map(|id| OmpEvent::Session(id.to_string()))
            .unwrap_or(OmpEvent::Other),
        "message_update" => {
            let Some(event) = json.get("assistantMessageEvent") else {
                return OmpEvent::Other;
            };
            match event.get("type").and_then(Value::as_str) {
                Some("text_delta") => event
                    .get("delta")
                    .and_then(Value::as_str)
                    .map(|delta| OmpEvent::TextDelta(delta.to_string()))
                    .unwrap_or(OmpEvent::Other),
                Some("text_end") => OmpEvent::FlushMarker,
                _ => OmpEvent::Other,
            }
        }
        "tool_execution_start" => {
            let Some(name) = json.get("toolName").and_then(Value::as_str) else {
                return OmpEvent::Other;
            };
            let args = json
                .get("args")
                .map(|args| serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string()))
                .unwrap_or_else(|| "{}".to_string());
            OmpEvent::ToolStart(name.to_string(), args)
        }
        "tool_execution_end" => OmpEvent::ToolResult {
            content: omp_result_text(&json),
            is_error: json
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "agent_end" => OmpEvent::FlushMarker,
        _ => OmpEvent::Other,
    }
}

/// Read the session id and working directory from an omp session JSONL file.
/// omp may write a title event before its session event, so the first few
/// records are scanned rather than assuming the header is the first line.
pub fn parse_omp_session_header(path: &Path) -> Option<(String, String)> {
    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(8) {
        let Ok(line) = line else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if json.get("type").and_then(Value::as_str) != Some("session") {
            continue;
        }
        let session_id = json.get("id")?.as_str()?.to_string();
        let cwd = json.get("cwd")?.as_str()?.to_string();
        return Some((session_id, cwd));
    }
    None
}

fn is_cancelled(cancel_token: Option<&std::sync::Arc<CancelToken>>) -> bool {
    cancel_token
        .is_some_and(|token| token.cancelled.load(std::sync::atomic::Ordering::Relaxed))
}

fn flush_text(sender: &Sender<StreamMessage>, pending: &mut String, pending_chars: &mut usize) {
    if pending.is_empty() {
        return;
    }
    let content = std::mem::take(pending);
    *pending_chars = 0;
    let _ = sender.send(StreamMessage::Text { content });
}

enum ReaderMessage {
    Line { raw: String, event: OmpEvent },
    Error(String),
}

/// Execute a prompt with the omp CLI and stream its JSONL events.
///
/// A new one-shot omp process is created for every message. Existing sessions
/// are resumed with `-r`; conversation persistence remains owned by omp.
pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    model: Option<&str>,
) -> Result<(), String> {
    omp_debug_log("========================================");
    omp_debug_log("=== omp execute_command_streaming START ===");
    omp_debug_log("========================================");
    omp_debug_log(&format!("prompt_len: {} chars", prompt.len()));
    omp_debug_log(&format!("session_id: {:?}", session_id));
    omp_debug_log(&format!("working_dir: {}", working_dir));
    omp_debug_log(&format!("model: {:?}", model));

    if let Some(session_id) = session_id {
        if !crate::services::process::is_valid_session_id(session_id) {
            return Err(format!("Invalid session_id format: {}", session_id));
        }
    }

    let mut args = vec![
        "-p".to_string(),
        "--mode".to_string(),
        "json".to_string(),
        "--no-title".to_string(),
        "--max-time".to_string(),
        "900".to_string(),
    ];

    if let Some(model) = model.filter(|model| !model.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(session_id) = session_id {
        args.push("-r".to_string());
        args.push(session_id.to_string());
    }

    let system_prompt_file: Option<PrivateTempFile> = match system_prompt.filter(|prompt| !prompt.is_empty()) {
        Some(system_prompt) => {
            let temp_dir = crate::utils::path::cokacdir_temp_dir()
                .map_err(|e| format!("Failed to prepare cokacdir temporary directory: {}", e))?;
            let guard = create_private_temp_file(&temp_dir, "omp_sp", system_prompt.as_bytes())
                .map_err(|e| {
                    omp_debug_log(&format!("[SP-FILE] ERROR: Failed to write system prompt file: {}", e));
                    format!("Failed to write system prompt file: {}", e)
                })?;
            Some(guard)
        }
        None => None,
    };
    if let Some(guard) = &system_prompt_file {
        let path = guard
            .verified_path()
            .map_err(|e| format!("Failed to verify system prompt file: {}", e))?;
        omp_debug_log(&format!("[SP-FILE] sp_path={:?}", path));
        args.push(format!("--append-system-prompt={}", path.to_string_lossy()));
    }

    // omp reads from a piped stdin until EOF. Keep stdin null and pass the
    // prompt as the final positional argument so launch can never deadlock.
    args.push(prompt.to_string());

    let omp_bin = get_omp_path().ok_or_else(|| {
        omp_debug_log("ERROR: omp CLI not found");
        "omp CLI not found. Is omp installed?".to_string()
    })?;
    omp_debug_log(&format!("Command: {} {:?}", omp_bin, args));

    let mut cmd = Command::new(omp_bin);
    cmd.args(&args)
        .current_dir(working_dir)
        .env(
            "PATH",
            crate::services::claude::enhanced_path_for_bin(omp_bin),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group (Unix) so kill_child_tree can SIGKILL the whole tree
    // (omp is a bun launcher with grandchildren); Linux cgroup cancel path.
    crate::services::claude::detach_into_own_pgroup(&mut cmd);
    crate::services::claude::attach_cancel_cgroup(&mut cmd, cancel_token.as_ref());
    let mut child = cmd.spawn().map_err(|e| {
        omp_debug_log(&format!("ERROR: Failed to spawn: {}", e));
        format!("Failed to start omp: {}. Is omp installed?", e)
    })?;
    omp_debug_log(&format!("omp process spawned, pid={:?}", child.id()));

    if let Some(token) = &cancel_token {
        let mut guard = token.child_pid.lock().unwrap_or_else(|error| error.into_inner());
        *guard = Some(child.id());
        drop(guard);
        if is_cancelled(cancel_token.as_ref()) {
            crate::services::claude::kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    let stderr_thread = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default())
    });
    let stdout = child.stdout.take().ok_or_else(|| {
        omp_debug_log("ERROR: Failed to capture stdout");
        "Failed to capture stdout".to_string()
    })?;
    let (reader_sender, reader_receiver) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(raw) => {
                    let event = classify_omp_json_line(&raw);
                    if reader_sender.send(ReaderMessage::Line { raw, event }).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = reader_sender.send(ReaderMessage::Error(error.to_string()));
                    break;
                }
            }
        }
    });

    let mut pending_text = String::new();
    let mut pending_chars = 0usize;
    let mut accumulated_text = String::new();
    let mut received_text = false;
    let mut new_session_id = None;
    let mut raw_stdout = String::new();
    let mut reader_error = None;

    loop {
        if is_cancelled(cancel_token.as_ref()) {
            omp_debug_log("Cancel detected — killing child process");
            crate::services::claude::kill_child_tree(&mut child);
            let _ = child.wait();
            let _ = reader_thread.join();
            if let Some(thread) = stderr_thread {
                let _ = thread.join();
            }
            return Ok(());
        }

        match reader_receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(ReaderMessage::Line { raw, event }) => {
                raw_stdout.push_str(&raw);
                raw_stdout.push('\n');
                match event {
                    OmpEvent::Session(session_id) => {
                        if new_session_id.is_none() {
                            omp_debug_log(&format!("Init: session_id={}", session_id));
                            new_session_id = Some(session_id.clone());
                            let _ = sender.send(StreamMessage::Init { session_id });
                        }
                    }
                    OmpEvent::TextDelta(delta) => {
                        if !delta.is_empty() {
                            received_text = true;
                        }
                        pending_chars += delta.chars().count();
                        accumulated_text.push_str(&delta);
                        pending_text.push_str(&delta);
                        if pending_chars >= 256 || pending_text.ends_with('\n') {
                            flush_text(&sender, &mut pending_text, &mut pending_chars);
                        }
                    }
                    OmpEvent::ToolStart(name, input) => {
                        let _ = sender.send(StreamMessage::ToolUse { name, input });
                    }
                    OmpEvent::ToolResult { content, is_error } => {
                        let _ = sender.send(StreamMessage::ToolResult { content, is_error });
                    }
                    OmpEvent::FlushMarker => {
                        flush_text(&sender, &mut pending_text, &mut pending_chars);
                    }
                    OmpEvent::Other => {}
                }
            }
            Ok(ReaderMessage::Error(error)) => {
                reader_error = Some(format!("Failed to read output: {}", error));
                crate::services::claude::kill_child_tree(&mut child);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                flush_text(&sender, &mut pending_text, &mut pending_chars);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = reader_thread.join();
    if is_cancelled(cancel_token.as_ref()) {
        omp_debug_log("Cancel detected after stdout EOF — killing child process");
        crate::services::claude::kill_child_tree(&mut child);
        let _ = child.wait();
        if let Some(thread) = stderr_thread {
            let _ = thread.join();
        }
        return Ok(());
    }

    let status = child.wait().map_err(|e| {
        omp_debug_log(&format!("ERROR: Process wait failed: {}", e));
        format!("Process error: {}", e)
    })?;
    let stderr = stderr_thread
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    omp_debug_log(&format!(
        "Process finished, exit_code: {:?}, text_received: {}, stderr_len: {}",
        status.code(), received_text, stderr.len()
    ));

    let process_failed = !status.success() && (!stderr.is_empty() || !received_text);
    if reader_error.is_some() || process_failed {
        let message = reader_error.unwrap_or_else(|| {
            format!("omp process exited with code {:?}", status.code())
        });
        let _ = sender.send(StreamMessage::Error {
            message: message.clone(),
            stdout: raw_stdout,
            stderr,
            exit_code: status.code(),
        });
        omp_debug_log(&format!("ERROR: {}", message));
        return Err(message);
    }

    flush_text(&sender, &mut pending_text, &mut pending_chars);
    let _ = sender.send(StreamMessage::AssistantFinal {
        content: accumulated_text.clone(),
    });
    let _ = sender.send(StreamMessage::Done {
        result: accumulated_text,
        session_id: new_session_id,
    });
    omp_debug_log("=== omp execute_command_streaming END (success) ===");
    Ok(())
    // system_prompt_file is dropped here and removes the private prompt file.
}

/// Verify whether an omp session's task has been fully completed.
pub fn verify_completion_omp(
    session_id: &str,
    working_dir: &str,
) -> Result<crate::services::claude::VerifyResult, String> {
    omp_debug_log("=== verify_completion_omp START ===");
    omp_debug_log(&format!("  session_id: {}", session_id));
    omp_debug_log(&format!("  working_dir: {}", working_dir));

    let transcript = crate::services::session_archive::build_verification_transcript(session_id)?;
    omp_debug_log(&format!("  transcript: {} chars", transcript.len()));
    let verify_prompt = format!(
        "Review the task transcript below. \
         Do NOT call any tools, do NOT read files, do NOT run commands — \
         judge purely from the transcript.\n\n\
         If the task appears fully and safely complete, respond with ONLY the single word: mission_complete\n\n\
         Otherwise respond with: mission_pending\n\
         followed by ONE short follow-up instruction (1–2 sentences).\n\n\
         CRITICAL — what this follow-up instruction IS:\n\
         The text you write after `mission_pending` will be taken verbatim and \
         delivered as the NEXT USER MESSAGE to the very same working agent that \
         produced the transcript. That agent will read it as if the user typed \
         it into the chat. Therefore write it as a direct, second-person \
         request from the user, not as a review/verdict/analysis.\n\n\
         The instruction should ask the agent to re-examine, re-verify, or \
         double-check whatever it just did — whatever form that work took. \
         Let the phrasing flow naturally from the actual work, not from a \
         fixed template.\n\n\
         Rules:\n\
         - Second-person imperative, as the user would type.\n\
         - NOT a diagnosis, NOT a checklist of missing items, NOT a summary \
           of what was done.\n\
         - Match the language of the transcript.\n\
         - 1–2 sentences. No preface, no \"I think\", no meta commentary.\n\n\
         === TRANSCRIPT ===\n{}\n=== END TRANSCRIPT ===",
        transcript
    );

    let omp_bin = get_omp_path().ok_or_else(|| {
        omp_debug_log("  ERROR: omp CLI not found");
        "omp CLI not found".to_string()
    })?;
    let args = [
        "-p",
        "--mode",
        "json",
        "--no-title",
        "--no-tools",
        "--max-time",
        "300",
        verify_prompt.as_str(),
    ];
    omp_debug_log(&format!("  args: {:?}", &args[..7]));
    let output = Command::new(omp_bin)
        .args(args)
        .current_dir(working_dir)
        .env(
            "PATH",
            crate::services::claude::enhanced_path_for_bin(omp_bin),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            omp_debug_log(&format!("  ERROR: Failed to spawn: {}", e));
            format!("Failed to start omp for verify_completion: {}", e)
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "verify_completion_omp process failed (exit {:?}). stderr: {}",
            output.status.code(),
            stderr.chars().take(500).collect::<String>()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut response_text = String::new();
    for line in stdout.lines() {
        if let OmpEvent::TextDelta(delta) = classify_omp_json_line(line) {
            response_text.push_str(&delta);
        }
    }
    if response_text.trim().is_empty() {
        return Err("verify_completion_omp produced no assistant text".to_string());
    }

    let complete = response_text.contains("mission_complete");
    let feedback = if complete {
        None
    } else {
        response_text
            .split_once("mission_pending")
            .map(|(_, feedback)| feedback.trim())
            .filter(|feedback| !feedback.is_empty())
            .map(ToString::to_string)
    };
    omp_debug_log(&format!(
        "  complete={}, feedback={:?}",
        complete, feedback
    ));
    omp_debug_log("=== verify_completion_omp END ===");
    Ok(crate::services::claude::VerifyResult { complete, feedback })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_omp_model_recognizes_provider_models() {
        assert!(is_omp_model(Some("omp")));
        assert!(is_omp_model(Some("omp:openai/gpt-5")));
        assert!(!is_omp_model(Some("openai/gpt-5")));
        assert!(!is_omp_model(Some("ompx:model")));
        assert!(!is_omp_model(None));
    }

    #[test]
    fn strip_omp_prefix_extracts_model_and_removes_display_suffix() {
        assert_eq!(strip_omp_prefix("omp"), None);
        assert_eq!(strip_omp_prefix("omp:"), None);
        assert_eq!(strip_omp_prefix("omp:openai/gpt-5"), Some("openai/gpt-5"));
        assert_eq!(
            strip_omp_prefix("omp:openai/gpt-5 — GPT-5"),
            Some("openai/gpt-5")
        );
        assert_eq!(strip_omp_prefix("claude:sonnet"), None);
    }

    #[test]
    fn parse_omp_session_header_skips_title_line() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"title\",\"title\":\"Test\"}\n",
                "{\"type\":\"session\",\"version\":3,\"id\":\"019fe3bb-test\",\"cwd\":\"/some/dir\"}\n"
            ),
        )
        .unwrap();

        assert_eq!(
            parse_omp_session_header(&path),
            Some(("019fe3bb-test".to_string(), "/some/dir".to_string()))
        );
    }

    #[test]
    fn classify_omp_session_event() {
        assert_eq!(
            classify_omp_json_line(
                r#"{"type":"session","version":3,"id":"019fe3bb-test","cwd":"/tmp"}"#
            ),
            OmpEvent::Session("019fe3bb-test".to_string())
        );
    }

    #[test]
    fn classify_omp_text_events_and_ignores_thinking() {
        assert_eq!(
            classify_omp_json_line(
                r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"hello"}}"#
            ),
            OmpEvent::TextDelta("hello".to_string())
        );
        assert_eq!(
            classify_omp_json_line(
                r#"{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":1,"content":"hello"}}"#
            ),
            OmpEvent::FlushMarker
        );
        assert_eq!(
            classify_omp_json_line(
                r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"secret"}}"#
            ),
            OmpEvent::Other
        );
    }

    #[test]
    fn classify_omp_tool_events_preserves_both_result_outcomes() {
        assert_eq!(
            classify_omp_json_line(
                r#"{"type":"tool_execution_start","toolCallId":"call-1","toolName":"write","args":{"path":"a.txt"},"intent":"write"}"#
            ),
            OmpEvent::ToolStart("write".to_string(), r#"{"path":"a.txt"}"#.to_string())
        );
        assert_eq!(
            classify_omp_json_line(
                r#"{"type":"tool_execution_end","toolName":"write","result":{"content":[{"type":"text","text":"Successfully wrote"}]},"isError":false}"#
            ),
            OmpEvent::ToolResult {
                content: "Successfully wrote".to_string(),
                is_error: false,
            }
        );
        assert_eq!(
            classify_omp_json_line(
                r#"{"type":"tool_execution_end","toolName":"write","result":{"content":[{"type":"text","text":"permission denied"}]},"isError":true}"#
            ),
            OmpEvent::ToolResult {
                content: "permission denied".to_string(),
                is_error: true,
            }
        );
    }

    #[test]
    fn classify_omp_flush_other_and_malformed_events() {
        assert_eq!(
            classify_omp_json_line(r#"{"type":"agent_end"}"#),
            OmpEvent::FlushMarker
        );
        for event_type in [
            "message_start",
            "message_end",
            "turn_start",
            "turn_end",
            "agent_start",
            "title",
            "model_change",
            "thinking_level_change",
            "tool_execution_update",
        ] {
            let line = format!(r#"{{"type":"{}"}}"#, event_type);
            assert_eq!(classify_omp_json_line(&line), OmpEvent::Other);
        }
        assert_eq!(classify_omp_json_line("not json"), OmpEvent::Other);
        assert_eq!(classify_omp_json_line("{}"), OmpEvent::Other);
    }

    #[test]
    #[ignore]
    fn integration_streams_real_omp() {
        if !is_omp_available() {
            eprintln!("omp CLI not available; skipping real integration test");
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let working_dir = temp.path().to_string_lossy().into_owned();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            execute_command_streaming(
                "Reply with exactly: HELLO_OMP",
                None,
                &working_dir,
                sender,
                Some("CRITICAL RULE: always reply in uppercase."),
                None,
                None,
            )
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        let mut init_session_id = None;
        let mut saw_text = false;
        let mut final_content = None;
        let mut done_session_id = None;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for omp stream");
            let message = receiver
                .recv_timeout(remaining)
                .expect("timed out waiting for omp stream");
            match message {
                StreamMessage::Init { session_id } => init_session_id = Some(session_id),
                StreamMessage::Text { .. } => saw_text = true,
                StreamMessage::AssistantFinal { content } => final_content = Some(content),
                StreamMessage::Done { session_id, .. } => {
                    done_session_id = session_id;
                    break;
                }
                StreamMessage::Error { message, stderr, .. } => {
                    panic!("real omp stream failed: {message}; stderr: {stderr}")
                }
                _ => {}
            }
        }

        worker.join().unwrap().unwrap();
        let init_session_id = init_session_id.expect("expected Init with session id");
        assert!(saw_text, "expected at least one streamed Text event");
        assert!(
            final_content
                .expect("expected AssistantFinal")
                .to_ascii_uppercase()
                .contains("HELLO_OMP")
        );
        assert_eq!(done_session_id.as_deref(), Some(init_session_id.as_str()));
    }

    /// Cancel must terminate the omp process tree promptly. Regression guard
    /// for the own-process-group requirement: without
    /// `detach_into_own_pgroup`, `kill_child_tree`'s `kill(-pid)` fails
    /// (ESRCH) and the stream stays blocked until the child exits naturally.
    #[test]
    #[ignore]
    fn cancel_terminates_omp_process_tree() {
        if !is_omp_available() {
            eprintln!("omp CLI not available; skipping real integration test");
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let working_dir = temp.path().to_string_lossy().into_owned();
        let (sender, _receiver) = mpsc::channel();
        let token = std::sync::Arc::new(CancelToken::new());
        let worker_token = std::sync::Arc::clone(&token);
        let worker = std::thread::spawn(move || {
            execute_command_streaming(
                "bash 도구로 `sleep 120`을 실행해. 실행 후 결과를 보고하지 마.",
                None,
                &working_dir,
                sender,
                None,
                Some(worker_token),
                None,
            )
        });

        // Wait until the child pid is registered, then give the agent time to
        // reach the long-running bash tool call (model turn + tool start).
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        let pid = loop {
            let guard = token.child_pid.lock().unwrap();
            if let Some(pid) = *guard {
                break pid;
            }
            drop(guard);
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for child pid registration"
            );
            std::thread::sleep(Duration::from_millis(200));
        };
        std::thread::sleep(Duration::from_secs(8));

        token.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        let cancel_start = std::time::Instant::now();
        let result = worker.join().expect("worker panicked");
        assert!(
            cancel_start.elapsed() < Duration::from_secs(20),
            "cancel did not terminate the stream promptly"
        );
        assert!(result.is_ok(), "execute_command_streaming failed: {:?}", result);

        // The whole process group (bun launcher + children) must be gone.
        let mut dead = false;
        for _ in 0..50 {
            let rc = unsafe { libc::kill(-(pid as libc::pid_t), 0) };
            if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                dead = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(dead, "omp process group (pid {}) still alive after cancel", pid);
    }
}
