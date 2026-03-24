//! Goose headless client for AI-assisted fix generation.
//!
//! Shells out to `goose run` with the developer extension to apply
//! complex migration fixes that can't be handled by pattern matching.

use anyhow::{Context, Result};
use frontend_core::fix::LlmFixRequest;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Per-file timeout for goose subprocess (seconds).
const GOOSE_TIMEOUT_SECS: u64 = 120;

/// Delay between consecutive goose calls to avoid rate limiting (seconds).
const GOOSE_DELAY_SECS: u64 = 2;

/// Maximum retries when a goose call times out.
const GOOSE_MAX_RETRIES: u32 = 1;

/// Result of a goose fix attempt.
#[derive(Debug)]
pub struct GooseFixResult {
    pub file_path: PathBuf,
    pub rule_id: String,
    pub success: bool,
    pub output: String,
}

/// Run a goose command with a timeout. Returns the combined stdout+stderr
/// output, or an error if the process times out or fails to start.
/// Return type from goose: (success, text_response, raw_json_output)
/// The raw_json_output contains the full goose session including all
/// tool calls, which is invaluable for debugging empty responses.
fn run_goose_with_timeout(prompt: &str, max_turns: &str) -> Result<(bool, String, String)> {
    let mut cmd = Command::new("goose");
    cmd.args([
        "run",
        "--quiet",
        "--text",
        prompt,
        "--with-builtin",
        "developer",
        "--no-session",
        "--max-turns",
        max_turns,
        "--output-format",
        "json",
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .stdin(std::process::Stdio::null());

    // Isolate goose in its own process group so that signals sent by
    // goose's child processes (e.g., claude-code) cannot propagate to
    // our parent process.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .context("Failed to execute goose. Is it installed and in PATH?")?;

    let timeout = Duration::from_secs(GOOSE_TIMEOUT_SECS);
    let start = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process exited — stdout is JSON with full message history
                let raw_json = child
                    .stdout
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok();
                        buf
                    })
                    .unwrap_or_default();
                let stderr = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok();
                        buf
                    })
                    .unwrap_or_default();

                // Extract the text response from the JSON output.
                // The JSON has { "messages": [ { "role": "assistant", "content": [...] } ] }
                // We want the last assistant message's text content.
                let text_response = extract_text_from_goose_json(&raw_json);

                return Ok((status.success(), text_response, raw_json));
            }
            Ok(None) => {
                // Still running — check timeout
                if start.elapsed() >= timeout {
                    // Kill the entire process group (goose + any children like
                    // claude-code) to prevent orphaned processes.
                    #[cfg(unix)]
                    {
                        let pid = child.id() as i32;
                        // SIGTERM first to allow graceful shutdown
                        unsafe {
                            libc::kill(-pid, libc::SIGTERM);
                        }
                        std::thread::sleep(Duration::from_millis(1000));
                        // SIGKILL to ensure cleanup
                        unsafe {
                            libc::kill(-pid, libc::SIGKILL);
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.kill();
                    }
                    let _ = child.wait();
                    anyhow::bail!("goose timed out after {}s", GOOSE_TIMEOUT_SECS);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                anyhow::bail!("Failed to wait on goose process: {}", e);
            }
        }
    }
}

/// Run goose fixes for all pending LLM requests.
/// Groups requests by file path for batch processing.
/// If `log_dir` is provided, saves prompts and responses to JSON files.
/// Maximum number of files to process concurrently.
/// Each file spawns a goose process, so this limits system load.
/// Determine the processing priority of a fix request within a batch.
///
/// Lower priority number = processed first. This ensures structural
/// migration rules (which require JSX restructuring) come before
/// informational/review-only rules.
fn fix_priority_by_id(id: &str) -> u8 {
    if id.contains("hierarchy-") {
        0 // hierarchy composition: highest priority, restructures component children
    } else if id.contains("component-import-deprecated") {
        1 // structural migration: removed/restructured components
    } else if id.contains("composition") || id.contains("new-sibling") {
        2 // composition: children→prop, new wrapper components
    } else if id.contains("removed")
        || id.contains("renamed")
        || id.contains("type-changed")
        || id.contains("signature-changed")
        || id.contains("prop-value")
    {
        3 // prop-level changes
    } else if id.contains("behavioral")
        || id.contains("dom-structure")
        || id.contains("css-")
        || id.contains("accessibility")
        || id.contains("logic-change")
        || id.contains("render-output")
    {
        4 // informational: DOM/CSS/a11y changes
    } else if id.starts_with("conformance-") {
        5 // review-only: conformance checks
    } else {
        3 // default: treat as prop-level
    }
}

/// An LLM fix request with multiple incidents from the same rule merged
/// into a single entry. This preserves the priority-based sort order
/// (hierarchy rules first) rather than re-sorting by rule_id.
#[derive(Debug)]
struct MergedLlmFixRequest {
    rule_id: String,
    file_path: PathBuf,
    /// All incident line numbers (may be a single line).
    lines: Vec<u32>,
    /// Rule message (shared across all incidents of the same rule).
    message: String,
    /// Code snippets keyed by line number.
    code_snips: Vec<(u32, String)>,
}

/// Merge LLM fix requests by rule_id, preserving insertion order.
///
/// Multiple incidents from the same rule (e.g., a composition rule firing
/// at different lines) are collapsed into a single entry with all affected
/// lines and code snippets combined.
fn merge_by_rule_id(requests: &[&LlmFixRequest]) -> Vec<MergedLlmFixRequest> {
    let mut merged: Vec<MergedLlmFixRequest> = Vec::new();
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();

    for req in requests {
        if let Some(&idx) = index.get(req.rule_id.as_str()) {
            merged[idx].lines.push(req.line);
            if let Some(snip) = &req.code_snip {
                merged[idx].code_snips.push((req.line, snip.clone()));
            }
        } else {
            let idx = merged.len();
            index.insert(&req.rule_id, idx);
            let code_snips = req
                .code_snip
                .as_ref()
                .map(|s| vec![(req.line, s.clone())])
                .unwrap_or_default();
            merged.push(MergedLlmFixRequest {
                rule_id: req.rule_id.clone(),
                file_path: req.file_path.clone(),
                lines: vec![req.line],
                message: req.message.clone(),
                code_snips,
            });
        }
    }

    merged
}

/// Extract the text response from goose's JSON output format.
///
/// Goose's `--output-format json` returns:
/// ```json
/// { "messages": [ { "role": "assistant", "content": [{ "type": "text", "text": "..." }] } ] }
/// ```
/// We extract the text from the LAST assistant message.
fn extract_text_from_goose_json(raw_json: &str) -> String {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(raw_json);
    match parsed {
        Ok(json) => {
            if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
                // Find the last assistant message
                for msg in messages.iter().rev() {
                    if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                        if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                            // Collect all text blocks
                            let texts: Vec<&str> = content
                                .iter()
                                .filter_map(|c| {
                                    if c.get("type").and_then(|t| t.as_str()) == Some("text") {
                                        c.get("text").and_then(|t| t.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            if !texts.is_empty() {
                                return texts.join("\n");
                            }
                        }
                    }
                }
                // Valid JSON with messages array but no assistant text —
                // goose ran but produced no output. Return empty so the
                // retry logic can detect this and retry.
                return String::new();
            }
            // No messages array at all — not goose JSON format.
            // Return as-is (might be plain text from older goose).
            raw_json.to_string()
        }
        Err(_) => {
            // Not valid JSON — return as-is (might be plain text from older goose)
            raw_json.to_string()
        }
    }
}

/// Maximum number of files to process concurrently.
/// Each file spawns a goose process, so this limits system load.
const MAX_CONCURRENT_FILES: usize = 3;

pub fn run_all_goose_fixes(
    requests: &[LlmFixRequest],
    verbose: bool,
    log_dir: Option<&std::path::Path>,
) -> Vec<GooseFixResult> {
    // Create log directory if specified
    if let Some(dir) = log_dir {
        let _ = std::fs::create_dir_all(dir);
    }

    // Group by file path for batching
    let mut by_file: BTreeMap<PathBuf, Vec<&LlmFixRequest>> = BTreeMap::new();
    for req in requests {
        by_file.entry(req.file_path.clone()).or_default().push(req);
    }

    // Merge incidents from the same rule within each file, then sort by
    // priority so the most impactful structural migration rules (hierarchy
    // composition) come first in each batch. This ensures:
    //  1. Multiple incidents from the same rule are presented as one fix.
    //  2. The first chunk starts with structural migration rules that
    //     trigger tool calls (file reads/edits), preventing empty goose output.
    //  3. Informational/review-only rules come last where they're less likely
    //     to consume turns or confuse the LLM.
    let mut merged_by_file: Vec<(PathBuf, Vec<MergedLlmFixRequest>)> = Vec::new();
    for (path, file_reqs) in by_file {
        let mut merged = merge_by_rule_id(&file_reqs);
        merged.sort_by(|a, b| fix_priority_by_id(&a.rule_id).cmp(&fix_priority_by_id(&b.rule_id)));
        merged_by_file.push((path, merged));
    }

    let total_files = merged_by_file.len();
    let total_fixes = requests.len();
    eprintln!(
        "  Processing {} fixes across {} files via goose ({} concurrent)...\n",
        total_fixes, total_files, MAX_CONCURRENT_FILES
    );

    let pipeline_start = std::time::Instant::now();
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let succeeded = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failed_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Use a channel as a simple concurrency limiter (semaphore)
    let (sem_tx, sem_rx) = std::sync::mpsc::sync_channel::<()>(MAX_CONCURRENT_FILES);
    for _ in 0..MAX_CONCURRENT_FILES {
        sem_tx.send(()).unwrap();
    }

    let file_entries: Vec<(usize, PathBuf, Vec<MergedLlmFixRequest>)> = merged_by_file
        .into_iter()
        .enumerate()
        .map(|(i, (path, reqs))| (i, path, reqs))
        .collect();

    let results: Vec<GooseFixResult> = std::thread::scope(|s| {
        let mut handles = Vec::new();

        for (i, file_path, file_requests) in &file_entries {
            // Acquire semaphore slot (blocks until a slot is free)
            sem_rx.recv().unwrap();

            let sem_tx = sem_tx.clone();
            let done = completed.clone();
            let ok_count = succeeded.clone();
            let fail_count = failed_count.clone();
            let i = *i;
            let log_dir = log_dir;

            let handle = s.spawn(move || {
                let result =
                    process_single_file(i, total_files, file_path, file_requests, verbose, log_dir);

                let idx = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                match &result {
                    r if r.success => {
                        ok_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    _ => {
                        fail_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                eprintln!("  [{}/{}] complete", idx, total_files,);

                // Release semaphore slot
                let _ = sem_tx.send(());
                result
            });

            handles.push(handle);
        }

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let total_elapsed = pipeline_start.elapsed();
    let ok = succeeded.load(std::sync::atomic::Ordering::Relaxed);
    let fail = failed_count.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "  Goose complete: {} succeeded, {} failed ({:.0}s total, {:.1}s avg per file)",
        ok,
        fail,
        total_elapsed.as_secs_f64(),
        total_elapsed.as_secs_f64() / total_files.max(1) as f64,
    );

    results
}

/// Process all fixes for a single file. Chunks are processed sequentially
/// within the file (each chunk reads the file as modified by the previous).
/// This function is called from parallel threads — one per file.
fn process_single_file(
    file_index: usize,
    total_files: usize,
    file_path: &PathBuf,
    file_requests: &[MergedLlmFixRequest],
    verbose: bool,
    log_dir: Option<&std::path::Path>,
) -> GooseFixResult {
    let file_name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| file_path.display().to_string());

    let rule_ids: Vec<&str> = file_requests.iter().map(|r| r.rule_id.as_str()).collect();
    let rules_display = if rule_ids.len() <= 3 {
        rule_ids.join(", ")
    } else {
        format!(
            "{}, ... +{} more",
            rule_ids[..2].join(", "),
            rule_ids.len() - 2
        )
    };

    eprintln!(
        "  [{}/{}] {} ({} fixes) [{}]",
        file_index + 1,
        total_files,
        file_name,
        file_requests.len(),
        rules_display,
    );

    let file_start = std::time::Instant::now();

    // Split large batches into chunks to avoid overwhelming the LLM.
    // Each chunk runs sequentially — the LLM reads the file as modified
    // by the previous chunk. A context summary of previously applied
    // fixes is prepended to each subsequent chunk.
    let max_fixes_per_batch = 8;
    let mut result: Result<GooseFixResult> = Ok(GooseFixResult {
        file_path: file_path.clone(),
        rule_id: String::new(),
        success: true,
        output: String::new(),
    });
    let mut all_prompts = Vec::new();
    let mut all_outputs: Vec<String> = Vec::new();
    let mut all_stderrs: Vec<String> = Vec::new();
    let mut chunk_times: Vec<f64> = Vec::new();
    let mut chunk_retried: Vec<bool> = Vec::new();
    let mut applied_summaries: Vec<String> = Vec::new();

    if file_requests.len() == 1 {
        let prompt = build_merged_prompt(&file_requests[0]);
        all_prompts.push(prompt.clone());
        let max_turns_str = "5".to_string();
        let mut goose_result = run_goose_with_timeout(&prompt, &max_turns_str);
        let mut was_retried = false;
        // Retry once on empty response
        if let Ok((_, ref output, _)) = goose_result {
            if output.len() <= 1 {
                was_retried = true;
                eprintln!("         {}: empty response — retrying once...", file_name,);
                std::thread::sleep(Duration::from_secs(2));
                goose_result = run_goose_with_timeout(&prompt, &max_turns_str);
            }
        }
        match goose_result {
            Ok((success, output, stderr)) => {
                all_outputs.push(output.clone());
                all_stderrs.push(stderr);
                chunk_times.push(0.0);
                chunk_retried.push(was_retried);
                result = Ok(GooseFixResult {
                    file_path: file_path.clone(),
                    rule_id: file_requests[0].rule_id.clone(),
                    success,
                    output,
                });
            }
            Err(e) => {
                result = Err(e);
            }
        }
    } else {
        let chunks: Vec<&[MergedLlmFixRequest]> =
            file_requests.chunks(max_fixes_per_batch).collect();
        let chunk_count = chunks.len();

        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            if chunk_idx > 0 {
                eprintln!(
                    "         {}: chunk {}/{} ({} fixes)...",
                    file_name,
                    chunk_idx + 1,
                    chunk_count,
                    chunk.len()
                );
            }

            let chunk_refs: Vec<&MergedLlmFixRequest> = chunk.iter().collect();
            let prompt = build_batch_prompt_with_context(
                file_path,
                &chunk_refs,
                if applied_summaries.is_empty() {
                    None
                } else {
                    Some(&applied_summaries)
                },
            );
            all_prompts.push(prompt.clone());

            let max_turns = (22 + chunk.len()).min(40);
            let max_turns_str = max_turns.to_string();
            let chunk_start = std::time::Instant::now();
            let mut chunk_result = run_goose_with_timeout(&prompt, &max_turns_str);
            let mut was_retried = false;

            // Retry once on empty response. Goose sometimes returns an
            // empty assistant message (no tool calls, no text) due to
            // transient LLM API issues or serialization failures.
            // NOTE: The first attempt may have made PARTIAL edits to the
            // file before failing to produce a summary. The retry prompt
            // accounts for this.
            if let Ok((_, ref output, _)) = chunk_result {
                if output.len() <= 1 {
                    was_retried = true;
                    eprintln!(
                        "         {}: chunk {}/{} empty response — retrying once...",
                        file_name,
                        chunk_idx + 1,
                        chunk_count,
                    );
                    std::thread::sleep(Duration::from_secs(2));
                    let retry_prompt = format!(
                        "{}\n\n\
                         RETRY: The previous attempt may have made PARTIAL changes to the file but did not complete. \
                         You MUST read the file as it exists NOW on disk, check each fix individually, \
                         and apply every fix that is not yet present. Do not assume all fixes are applied \
                         just because some are — check EVERY one.",
                        prompt,
                    );
                    chunk_result = run_goose_with_timeout(&retry_prompt, &max_turns_str);
                }
            }

            let chunk_elapsed = chunk_start.elapsed();

            match chunk_result {
                Ok((success, output, stderr)) => {
                    let resp_len = output.len();
                    let status = if resp_len <= 1 {
                        "EMPTY"
                    } else if success {
                        "ok"
                    } else {
                        "FAILED"
                    };
                    // Count messages in the goose JSON to understand what happened
                    let msg_count = serde_json::from_str::<serde_json::Value>(&stderr)
                        .ok()
                        .and_then(|j| j.get("messages")?.as_array().map(|a| a.len()))
                        .unwrap_or(0);

                    eprintln!(
                        "         {}: chunk {}/{} {} ({} fixes, {:.1}s, response={} chars, goose_messages={})",
                        file_name,
                        chunk_idx + 1,
                        chunk_count,
                        status,
                        chunk.len(),
                        chunk_elapsed.as_secs_f64(),
                        resp_len,
                        msg_count,
                    );
                    // If empty response, summarize what goose did from the JSON
                    if resp_len <= 1 {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stderr) {
                            if let Some(messages) = json.get("messages").and_then(|m| m.as_array())
                            {
                                for msg in messages {
                                    let role =
                                        msg.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                                    if role == "assistant" {
                                        if let Some(content) =
                                            msg.get("content").and_then(|c| c.as_array())
                                        {
                                            for item in content {
                                                let typ = item
                                                    .get("type")
                                                    .and_then(|t| t.as_str())
                                                    .unwrap_or("?");
                                                if typ == "toolUse" {
                                                    let tool = item
                                                        .get("name")
                                                        .and_then(|n| n.as_str())
                                                        .unwrap_or("?");
                                                    eprintln!(
                                                        "           goose: tool_call={}",
                                                        tool
                                                    );
                                                } else if typ == "text" {
                                                    let text = item
                                                        .get("text")
                                                        .and_then(|t| t.as_str())
                                                        .unwrap_or("");
                                                    if !text.is_empty() {
                                                        eprintln!(
                                                            "           goose: text={}",
                                                            &text[..text.len().min(100)]
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        } else if msg_count == 0 {
                            eprintln!("           goose: no messages in output (goose may have failed silently)");
                        }
                    }

                    chunk_times.push(chunk_elapsed.as_secs_f64());
                    chunk_retried.push(was_retried);
                    all_stderrs.push(stderr);

                    // Record what was applied for context in next chunk
                    for req in chunk.iter() {
                        let lines_display = req
                            .lines
                            .iter()
                            .map(|l| l.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        applied_summaries.push(format!(
                            "- {} (line {}): {}",
                            req.rule_id,
                            lines_display,
                            req.message.lines().next().unwrap_or("")
                        ));
                    }
                    all_outputs.push(output.clone());
                    result = Ok(GooseFixResult {
                        file_path: file_path.clone(),
                        rule_id: file_requests
                            .iter()
                            .map(|r| r.rule_id.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        success,
                        output,
                    });
                    if !success {
                        eprintln!(
                            "         {}: chunk {}/{} FAILED, stopping",
                            file_name,
                            chunk_idx + 1,
                            chunk_count
                        );
                        break;
                    }
                }
                Err(e) => {
                    // Retry on timeout
                    let err_msg = format!("{}", e);
                    if err_msg.contains("timed out") {
                        let backoff = Duration::from_secs(10);
                        eprintln!(
                            "         {}: chunk {}/{} timed out after {:.1}s, retrying in {}s...",
                            file_name,
                            chunk_idx + 1,
                            chunk_count,
                            chunk_elapsed.as_secs_f64(),
                            backoff.as_secs(),
                        );
                        std::thread::sleep(backoff);
                        let retry_start = std::time::Instant::now();
                        let retry_result = run_goose_with_timeout(&prompt, &max_turns_str);
                        match retry_result {
                            Ok((success, output, retry_stderr)) => {
                                for req in chunk.iter() {
                                    let lines_display = req
                                        .lines
                                        .iter()
                                        .map(|l| l.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    applied_summaries.push(format!(
                                        "- {} (line {}): {}",
                                        req.rule_id,
                                        lines_display,
                                        req.message.lines().next().unwrap_or("")
                                    ));
                                }
                                all_outputs.push(output.clone());
                                result = Ok(GooseFixResult {
                                    file_path: file_path.clone(),
                                    rule_id: file_requests
                                        .iter()
                                        .map(|r| r.rule_id.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    success,
                                    output,
                                });
                            }
                            Err(e2) => {
                                result = Err(e2);
                                break;
                            }
                        }
                    } else {
                        result = Err(e);
                        break;
                    }
                }
            }
        }
    }

    let elapsed = file_start.elapsed();

    match result {
        Ok(r) => {
            if r.success {
                eprintln!("         {}: ok ({:.1}s)", file_name, elapsed.as_secs_f64());
            } else {
                eprintln!(
                    "         {}: FAILED ({:.1}s)",
                    file_name,
                    elapsed.as_secs_f64()
                );
            }
            if verbose && !r.output.is_empty() {
                for line in r.output.lines().take(5) {
                    eprintln!("           {}", line);
                }
            }

            // Save all prompts + responses to log file (one entry per chunk)
            if let Some(dir) = log_dir {
                let chunks: Vec<serde_json::Value> = all_prompts
                    .iter()
                    .enumerate()
                    .map(|(i, prompt)| {
                        let resp = all_outputs.get(i).unwrap_or(&String::new()).clone();
                        let raw_json = all_stderrs.get(i).unwrap_or(&String::new()).clone();
                        let resp_len = resp.len();
                        // Parse the raw goose JSON for structured logging
                        let goose_session: serde_json::Value = serde_json::from_str(&raw_json)
                            .unwrap_or_else(|_| serde_json::json!({"raw": raw_json}));
                        serde_json::json!({
                            "chunk": i + 1,
                            "prompt": prompt,
                            "response": resp,
                            "response_length": resp_len,
                            "elapsed_secs": chunk_times.get(i).unwrap_or(&0.0),
                            "retried": chunk_retried.get(i).unwrap_or(&false),
                            "status": if resp_len <= 1 { "empty" } else { "ok" },
                            "goose_session": goose_session,
                        })
                    })
                    .collect();

                let log_entry = serde_json::json!({
                    "file": file_path.display().to_string(),
                    "rule_ids": file_requests.iter().map(|r| &r.rule_id).collect::<Vec<_>>(),
                    "chunks": chunks,
                    "total_chunks": all_prompts.len(),
                    "success": r.success,
                    "elapsed_secs": elapsed.as_secs_f64(),
                });
                let log_file = dir.join(format!("goose-fix-{:03}.json", file_index + 1));
                let _ = std::fs::write(
                    &log_file,
                    serde_json::to_string_pretty(&log_entry).unwrap_or_default(),
                );
            }

            r
        }
        Err(e) => {
            eprintln!(
                "         {}: ERROR ({:.1}s) — {}",
                file_name,
                elapsed.as_secs_f64(),
                e
            );
            GooseFixResult {
                file_path: file_path.clone(),
                rule_id: file_requests
                    .iter()
                    .map(|r| r.rule_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                success: false,
                output: format!("Error: {}", e),
            }
        }
    }
}

// ── Prompt construction ───────────────────────────────────────────────────

/// Build a prompt for a single merged fix request (one unique rule, possibly
/// multiple incident lines).
fn build_merged_prompt(request: &MergedLlmFixRequest) -> String {
    let lines_display = request
        .lines
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let mut code_context = String::new();
    if request.code_snips.is_empty() {
        code_context.push_str("(no code snippet available)");
    } else if request.code_snips.len() == 1 {
        code_context.push_str(&request.code_snips[0].1);
    } else {
        for (line, snip) in &request.code_snips {
            code_context.push_str(&format!("  (line {}):\n{}\n", line, snip));
        }
    }

    format!(
        r#"You are applying a PatternFly v5 to v6 migration fix.

File: {file_path}
Line: {lines}

Migration rule [{rule_id}]:
{message}

Code context:
```
{code_context}
```

Instructions:
1. Read the file at {file_path}
2. Apply ONLY the change described by the migration rule at or near line {lines}
3. Make the minimum edit necessary — do not change unrelated code
4. Write the fixed file

IMPORTANT constraints:
- NEVER use deep import paths like '@patternfly/react-core/dist/esm/...' or '@patternfly/react-core/next'. Always use the public barrel import '@patternfly/react-core'.
- NEVER replace PatternFly components (Button, MenuToggle, etc.) with raw HTML elements (<button>, <a>, <div>). If a component still exists in PF6, keep using it.
- NEVER remove data-ouia-component-id, ouiaId, or other test identifier props unless the migration rule specifically says to.
- NEVER invent or use component names that are not mentioned in the migration rules or already imported in the file. Only use components explicitly named in the rule message.
- When adding new components (ModalHeader, ModalBody, ModalFooter, etc.), import them from the same package as the parent component.
- If the migration rule says a component was "restructured" or "still exists", keep the component and only restructure its props/children as described. If the rule says a component was "removed" and tells you to remove the import, DO remove it and migrate to the replacement described in the rule.
- When a prop migration says to pass a prop to a child component (e.g., 'actions → pass as children of <ModalFooter>'), you MUST create that child component element, import it, and render the prop value within it.
- Props can be passed in multiple ways — look for ALL of them when migrating a removed prop:
  * Direct: `propName={{value}}`
  * Conditional spread: `{{...(value && {{ propName: value }})}}` or `{{...(condition && {{ propName }})}}` — convert the condition to wrap the new child component, e.g., `{{value && <ChildComponent>{{value}}</ChildComponent>}}`
  * Object spread: `{{...props}}` — check if the spread object contains the removed prop

Before writing, reason through the fix step by step to ensure nothing is missed. Then read the file, make the edit, and write it."#,
        file_path = request.file_path.display(),
        lines = lines_display,
        rule_id = request.rule_id,
        message = request.message,
        code_context = code_context,
    )
}

fn build_batch_prompt_with_context(
    file_path: &PathBuf,
    requests: &[&MergedLlmFixRequest],
    previously_applied: Option<&[String]>,
) -> String {
    // Requests are already merged by rule_id and sorted by priority upstream
    // (in run_all_goose_fixes). We iterate directly in the provided order
    // so that hierarchy composition rules (priority 0) appear first in the
    // prompt, giving them the most LLM attention.
    let mut fixes = String::new();
    for (idx, req) in requests.iter().enumerate() {
        let fix_num = idx + 1;
        let lines_display = req
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(", ");

        if req.lines.len() == 1 {
            // Single incident
            let code_context = req
                .code_snips
                .first()
                .map(|(_, s)| s.as_str())
                .unwrap_or("(no snippet)");
            fixes.push_str(&format!(
                r#"
### Fix {num}
Line: {line}
Rule [{rule_id}]:
{message}

Code context:
```
{code_context}
```
"#,
                num = fix_num,
                line = lines_display,
                rule_id = req.rule_id,
                message = req.message,
                code_context = code_context,
            ));
        } else {
            // Multiple incidents from the same rule
            let mut all_snippets = String::new();
            for (line, snip) in &req.code_snips {
                all_snippets.push_str(&format!("  (line {}):\n{}\n", line, snip));
            }
            fixes.push_str(&format!(
                r#"
### Fix {num}
Lines: {lines}
Rule [{rule_id}]:
{message}

This rule affects multiple locations in the file. Apply ALL steps together as one logical change.

Code contexts:
```
{all_snippets}```
"#,
                num = fix_num,
                lines = lines_display,
                rule_id = req.rule_id,
                message = req.message,
                all_snippets = all_snippets,
            ));
        }
    }

    let context_section = if let Some(applied) = previously_applied {
        format!(
            "\n## Previously attempted fixes:\n\
             The following fixes were attempted in a previous pass. Most should already\n\
             be applied to the file on disk. Do NOT revert changes that are already correct.\n\
             However, if any of these fixes were NOT actually applied (the old pattern\n\
             still exists in the file), apply them now along with the new fixes below.\n\
             {}\n\n",
            applied.join("\n")
        )
    } else {
        String::new()
    };

    format!(
        r#"You are applying PatternFly v5 to v6 migration fixes to a single file.

File: {file_path}
{context_section}
Apply ALL of the following {count} fixes to this file:
{fixes}
Instructions:
1. Read the file at {file_path}
2. Process each fix INDEPENDENTLY in sequence. For each fix:
   a. Identify the exact code affected (line number, prop, component)
   b. Determine the specific change needed (add/remove/move import, restructure JSX, migrate prop)
   c. If a prop is being relocated to a child component, note the child component that must be created
   d. Track the change for the final write
3. Make the minimum edits necessary — do not change unrelated code
4. Do NOT revert any changes that were already applied in previous passes
5. Write the fixed file once with ALL changes from every fix applied

IMPORTANT constraints:
- NEVER use deep import paths like '@patternfly/react-core/dist/esm/...' or '@patternfly/react-core/next'. Always use the public barrel import '@patternfly/react-core'.
- NEVER replace PatternFly components (Button, MenuToggle, etc.) with raw HTML elements (<button>, <a>, <div>). If a component still exists in PF6, keep using it.
- NEVER remove data-ouia-component-id, ouiaId, or other test identifier props unless the migration rule specifically says to.
- NEVER invent or use component names that are not mentioned in the migration rules or already imported in the file. Only use components explicitly named in the rule message.
- When adding new components (ModalHeader, ModalBody, ModalFooter, etc.), import them from the same package as the parent component.
- If the migration rule says a component was "restructured" or "still exists", keep the component and only restructure its props/children as described. If the rule says a component was "removed" and tells you to remove the import, DO remove it and migrate to the replacement described in the rule.
- When a prop migration says to pass a prop to a child component (e.g., 'actions → pass as children of <ModalFooter>'), you MUST create that child component element, import it, and render the prop value within it.
- Props can be passed in multiple ways — look for ALL of them when migrating a removed prop:
  * Direct: `propName={{value}}`
  * Conditional spread: `{{...(value && {{ propName: value }})}}` or `{{...(condition && {{ propName }})}}` — convert the condition to wrap the new child component, e.g., `{{value && <ChildComponent>{{value}}</ChildComponent>}}`
  * Object spread: `{{...props}}` — check if the spread object contains the removed prop

VERIFICATION: After making edits, check that EVERY removed prop listed in the migration rules
has been migrated to its specified child component. Do NOT declare a migration "already applied"
unless ALL listed child components are present AND all removed props are accounted for.

Before writing, reason through each fix step by step to ensure nothing is missed. Then read the file, make the edits, and write it."#,
        file_path = file_path.display(),
        context_section = context_section,
        count = requests.len(),
        fixes = fixes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_req(rule_id: &str) -> LlmFixRequest {
        make_req_at_line(rule_id, 1, None)
    }

    fn make_req_at_line(rule_id: &str, line: u32, code_snip: Option<&str>) -> LlmFixRequest {
        LlmFixRequest {
            file_path: PathBuf::from("/tmp/test.tsx"),
            file_uri: "file:///tmp/test.tsx".to_string(),
            line,
            rule_id: rule_id.to_string(),
            message: format!("Migration for {}", rule_id),
            code_snip: code_snip.map(|s| s.to_string()),
            source: None,
        }
    }

    #[test]
    fn test_fix_priority_structural_first() {
        assert_eq!(
            fix_priority_by_id("semver-modal-component-import-deprecated"),
            1
        );
        assert_eq!(
            fix_priority_by_id("semver-composition-button-children-to-icon-prop"),
            2
        );
        assert_eq!(
            fix_priority_by_id("semver-packages-react-core-modalprops-title-removed"),
            3
        );
        assert_eq!(
            fix_priority_by_id("semver-packages-react-core-button-tsx-button-behavioral"),
            4
        );
        assert_eq!(
            fix_priority_by_id("conformance-dropdown-expected-children"),
            5
        );
    }

    #[test]
    fn test_fix_priority_hierarchy_is_highest() {
        assert_eq!(
            fix_priority_by_id("semver-hierarchy-modal-composition-changed"),
            0
        );
    }

    #[test]
    fn test_fix_priority_sorting_order() {
        let mut ids = vec![
            "conformance-dropdown-expected-children",
            "semver-packages-react-core-button-behavioral",
            "semver-modal-component-import-deprecated",
            "semver-composition-button-children-to-icon-prop",
            "semver-packages-react-core-modalprops-title-removed",
            "semver-hierarchy-emptystate-composition-changed",
            "conformance-toolbar-expected-children",
            "semver-packages-react-core-dom-structure",
        ];

        ids.sort_by(|a, b| fix_priority_by_id(a).cmp(&fix_priority_by_id(b)));

        // Hierarchy (0) and component-import-deprecated (1) come first
        assert!(ids[0].contains("component-import-deprecated") || ids[0].contains("hierarchy-"));
        assert!(ids[1].contains("component-import-deprecated") || ids[1].contains("hierarchy-"));

        // Conformance (5) comes last
        assert!(ids[ids.len() - 1].starts_with("conformance-"));
        assert!(ids[ids.len() - 2].starts_with("conformance-"));
    }

    #[test]
    fn test_fix_priority_dom_and_css_are_informational() {
        assert_eq!(fix_priority_by_id("semver-foo-dom-structure"), 4);
        assert_eq!(fix_priority_by_id("semver-foo-css-class"), 4);
        assert_eq!(fix_priority_by_id("semver-foo-css-variable"), 4);
        assert_eq!(fix_priority_by_id("semver-foo-accessibility"), 4);
        assert_eq!(fix_priority_by_id("semver-foo-logic-change"), 4);
        assert_eq!(fix_priority_by_id("semver-foo-render-output"), 4);
    }

    #[test]
    fn test_fix_priority_prop_level_changes() {
        assert_eq!(fix_priority_by_id("semver-foo-removed"), 3);
        assert_eq!(fix_priority_by_id("semver-foo-renamed"), 3);
        assert_eq!(fix_priority_by_id("semver-foo-type-changed"), 3);
        assert_eq!(fix_priority_by_id("semver-foo-signature-changed"), 3);
        assert_eq!(fix_priority_by_id("semver-foo-prop-value-change"), 3);
    }

    #[test]
    fn test_fix_priority_unknown_defaults_to_prop_level() {
        assert_eq!(fix_priority_by_id("semver-something-unknown"), 3);
    }

    // ── merge_by_rule_id tests ───────────────────────────────────────────

    #[test]
    fn test_merge_by_rule_id_no_duplicates() {
        let reqs = vec![make_req("rule-a"), make_req("rule-b"), make_req("rule-c")];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();
        let merged = merge_by_rule_id(&refs);

        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].rule_id, "rule-a");
        assert_eq!(merged[1].rule_id, "rule-b");
        assert_eq!(merged[2].rule_id, "rule-c");
        assert_eq!(merged[0].lines, vec![1]);
        assert_eq!(merged[1].lines, vec![1]);
        assert_eq!(merged[2].lines, vec![1]);
    }

    #[test]
    fn test_merge_by_rule_id_combines_same_rule() {
        let reqs = vec![
            make_req_at_line("rule-a", 7, Some("line 7 code")),
            make_req_at_line("rule-b", 10, None),
            make_req_at_line("rule-a", 152, Some("line 152 code")),
        ];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();
        let merged = merge_by_rule_id(&refs);

        assert_eq!(merged.len(), 2);
        // rule-a appears first (first occurrence)
        assert_eq!(merged[0].rule_id, "rule-a");
        assert_eq!(merged[0].lines, vec![7, 152]);
        assert_eq!(merged[0].code_snips.len(), 2);
        assert_eq!(merged[0].code_snips[0], (7, "line 7 code".to_string()));
        assert_eq!(merged[0].code_snips[1], (152, "line 152 code".to_string()));
        // rule-b is second
        assert_eq!(merged[1].rule_id, "rule-b");
        assert_eq!(merged[1].lines, vec![10]);
    }

    #[test]
    fn test_merge_preserves_insertion_order() {
        // Simulate the order after priority sort: hierarchy first, then
        // composition, then prop-level, then conformance.
        let reqs = vec![
            make_req_at_line("semver-hierarchy-modal-composition-changed", 9, None),
            make_req_at_line("semver-hierarchy-emptystate-composition-changed", 6, None),
            make_req_at_line("semver-composition-button-children-to-icon-prop", 139, None),
            make_req_at_line(
                "semver-composition-emptystateheader-nesting-changed",
                152,
                None,
            ),
            make_req_at_line(
                "semver-emptystateheader-component-import-deprecated",
                7,
                None,
            ),
            make_req_at_line(
                "semver-composition-emptystateheader-nesting-changed",
                7,
                None,
            ), // dup at different line
            make_req_at_line("conformance-table-expected-children", 14, None),
        ];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();
        let merged = merge_by_rule_id(&refs);

        // 6 unique rules (emptystateheader-nesting-changed merges two lines)
        assert_eq!(merged.len(), 6);
        // Verify order matches insertion (priority) order
        assert_eq!(
            merged[0].rule_id,
            "semver-hierarchy-modal-composition-changed"
        );
        assert_eq!(
            merged[1].rule_id,
            "semver-hierarchy-emptystate-composition-changed"
        );
        assert_eq!(
            merged[2].rule_id,
            "semver-composition-button-children-to-icon-prop"
        );
        assert_eq!(
            merged[3].rule_id,
            "semver-composition-emptystateheader-nesting-changed"
        );
        assert_eq!(merged[3].lines, vec![152, 7]); // both lines preserved
        assert_eq!(
            merged[4].rule_id,
            "semver-emptystateheader-component-import-deprecated"
        );
        assert_eq!(merged[5].rule_id, "conformance-table-expected-children");
    }

    #[test]
    fn test_merge_then_sort_produces_correct_prompt_order() {
        // This is the key test: simulates the exact scenario from the bug.
        // The input is unsorted (as it comes from kantra violations).
        // After merge + sort, hierarchy rules must be FIRST, not buried
        // at Fix 7/8 like the BTreeMap bug caused.
        let reqs = vec![
            make_req_at_line("semver-composition-button-children-to-icon-prop", 139, None),
            make_req_at_line("semver-composition-button-nesting-changed", 139, None),
            make_req_at_line(
                "semver-composition-emptystateheader-nesting-changed",
                152,
                None,
            ),
            make_req_at_line(
                "semver-composition-emptystateicon-nesting-changed",
                152,
                None,
            ),
            make_req_at_line(
                "semver-emptystateheader-component-import-deprecated",
                7,
                None,
            ),
            make_req_at_line("semver-emptystateicon-component-import-deprecated", 8, None),
            make_req_at_line("semver-hierarchy-emptystate-composition-changed", 6, None),
            make_req_at_line("semver-hierarchy-modal-composition-changed", 9, None),
        ];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();

        // Step 1: merge by rule_id (preserves insertion order)
        let mut merged = merge_by_rule_id(&refs);

        // Step 2: sort by priority (hierarchy=0 first, conformance=5 last)
        merged.sort_by(|a, b| fix_priority_by_id(&a.rule_id).cmp(&fix_priority_by_id(&b.rule_id)));

        // Hierarchy rules (priority 0) must be first
        assert_eq!(
            merged[0].rule_id,
            "semver-hierarchy-emptystate-composition-changed"
        );
        assert_eq!(
            merged[1].rule_id,
            "semver-hierarchy-modal-composition-changed"
        );

        // Component-import-deprecated (priority 1) next
        assert_eq!(
            merged[2].rule_id,
            "semver-emptystateheader-component-import-deprecated"
        );
        assert_eq!(
            merged[3].rule_id,
            "semver-emptystateicon-component-import-deprecated"
        );

        // Composition (priority 2) after that
        assert!(merged[4].rule_id.contains("composition"));
        assert!(merged[5].rule_id.contains("composition"));
        assert!(merged[6].rule_id.contains("composition"));
        assert!(merged[7].rule_id.contains("composition"));
    }

    #[test]
    fn test_batch_prompt_preserves_priority_order() {
        // Verify that the prompt Fix numbering follows priority order,
        // not alphabetical order. This is the exact bug that was fixed.
        let reqs = vec![
            make_req("semver-composition-button-children-to-icon-prop"),
            make_req("semver-hierarchy-modal-composition-changed"),
            make_req("conformance-table-expected-children"),
        ];
        let refs: Vec<&LlmFixRequest> = reqs.iter().collect();

        // Merge and sort by priority
        let mut merged = merge_by_rule_id(&refs);
        merged.sort_by(|a, b| fix_priority_by_id(&a.rule_id).cmp(&fix_priority_by_id(&b.rule_id)));

        // Build the prompt
        let merged_refs: Vec<&MergedLlmFixRequest> = merged.iter().collect();
        let prompt =
            build_batch_prompt_with_context(&PathBuf::from("/tmp/test.tsx"), &merged_refs, None);

        // Extract Fix N and corresponding rule IDs from the prompt
        let fix_rules: Vec<&str> = prompt
            .lines()
            .filter(|l| l.starts_with("Rule ["))
            .map(|l| l.trim_start_matches("Rule [").trim_end_matches("]:"))
            .collect();

        // Fix 1 must be the hierarchy rule (priority 0), not the
        // composition rule that sorts earlier alphabetically
        assert_eq!(fix_rules[0], "semver-hierarchy-modal-composition-changed");
        assert_eq!(
            fix_rules[1],
            "semver-composition-button-children-to-icon-prop"
        );
        assert_eq!(fix_rules[2], "conformance-table-expected-children");
    }

    #[test]
    fn test_extract_text_from_goose_json_valid() {
        let json = r#"{
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "The file has been updated."}]}
            ]
        }"#;
        let result = extract_text_from_goose_json(json);
        assert_eq!(result, "The file has been updated.");
    }

    #[test]
    fn test_extract_text_from_goose_json_empty_messages() {
        let json = r#"{"messages": []}"#;
        let result = extract_text_from_goose_json(json);
        // Returns empty so retry logic can detect the failure
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_text_from_goose_json_user_only_no_assistant() {
        let json =
            r#"{"messages": [{"role": "user", "content": [{"type": "text", "text": "prompt"}]}]}"#;
        let result = extract_text_from_goose_json(json);
        // User message but no assistant response — return empty for retry
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_text_from_goose_json_empty_object() {
        let json = "{}";
        let result = extract_text_from_goose_json(json);
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_extract_text_from_goose_json_not_json() {
        let text = "This is plain text output from goose";
        let result = extract_text_from_goose_json(text);
        assert_eq!(result, text);
    }

    #[test]
    fn test_extract_text_from_goose_json_multiple_text_blocks() {
        let json = r#"{
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "First part."},
                    {"type": "toolUse", "name": "developer__read_file"},
                    {"type": "text", "text": "Second part."}
                ]}
            ]
        }"#;
        let result = extract_text_from_goose_json(json);
        assert_eq!(result, "First part.\nSecond part.");
    }
}
