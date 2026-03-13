//! Goose headless client for AI-assisted fix generation.
//!
//! Shells out to `goose run` with the developer extension to apply
//! complex migration fixes that can't be handled by pattern matching.

use anyhow::{Context, Result};
use frontend_core::fix::LlmFixRequest;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

/// Result of a goose fix attempt.
#[derive(Debug)]
pub struct GooseFixResult {
    pub file_path: PathBuf,
    pub rule_id: String,
    pub success: bool,
    pub output: String,
}

/// Run goose to fix a single incident.
pub fn run_goose_fix(request: &LlmFixRequest) -> Result<GooseFixResult> {
    let prompt = build_prompt(request);

    let output = Command::new("goose")
        .args([
            "run",
            "--quiet",
            "--text",
            &prompt,
            "--with-builtin",
            "developer",
            "--no-session",
            "--max-turns",
            "5",
        ])
        .output()
        .context("Failed to execute goose. Is it installed and in PATH?")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = if stderr.is_empty() {
        stdout.clone()
    } else {
        format!("{}\n{}", stdout, stderr)
    };

    Ok(GooseFixResult {
        file_path: request.file_path.clone(),
        rule_id: request.rule_id.clone(),
        success: output.status.success(),
        output: combined,
    })
}

/// Run goose to fix multiple incidents in the same file (batched).
pub fn run_goose_fix_batch(
    file_path: &PathBuf,
    requests: &[&LlmFixRequest],
) -> Result<GooseFixResult> {
    let prompt = build_batch_prompt(file_path, requests);

    let output = Command::new("goose")
        .args([
            "run",
            "--quiet",
            "--text",
            &prompt,
            "--with-builtin",
            "developer",
            "--no-session",
            "--max-turns",
            "8",
        ])
        .output()
        .context("Failed to execute goose. Is it installed and in PATH?")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = if stderr.is_empty() {
        stdout.clone()
    } else {
        format!("{}\n{}", stdout, stderr)
    };

    Ok(GooseFixResult {
        file_path: file_path.clone(),
        rule_id: requests
            .iter()
            .map(|r| r.rule_id.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        success: output.status.success(),
        output: combined,
    })
}

/// Run goose fixes for all pending LLM requests.
/// Groups requests by file path for batch processing.
/// If `log_dir` is provided, saves prompts and responses to JSON files.
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

    let total_files = by_file.len();
    let total_fixes = requests.len();
    eprintln!(
        "  Processing {} fixes across {} files via goose...\n",
        total_fixes, total_files
    );

    let mut results = Vec::new();
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let pipeline_start = std::time::Instant::now();

    for (i, (file_path, file_requests)) in by_file.iter().enumerate() {
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
            "  [{}/{}] {} ({} fixes)",
            i + 1,
            total_files,
            file_name,
            file_requests.len()
        );
        eprintln!("         rules: {}", rules_display);
        eprint!("         goose: running...");

        let file_start = std::time::Instant::now();

        let result = if file_requests.len() == 1 {
            run_goose_fix(file_requests[0])
        } else {
            run_goose_fix_batch(file_path, file_requests)
        };

        let elapsed = file_start.elapsed();

        // Build the prompt for logging
        let prompt_text = if file_requests.len() == 1 {
            build_prompt(file_requests[0])
        } else {
            build_batch_prompt(file_path, &file_requests)
        };

        match result {
            Ok(r) => {
                if r.success {
                    succeeded += 1;
                    eprintln!("\r         goose: ok ({:.1}s)", elapsed.as_secs_f64());
                } else {
                    failed += 1;
                    eprintln!("\r         goose: FAILED ({:.1}s)", elapsed.as_secs_f64());
                }
                if verbose && !r.output.is_empty() {
                    for line in r.output.lines().take(5) {
                        eprintln!("           {}", line);
                    }
                }

                // Save prompt + response to log file
                if let Some(dir) = log_dir {
                    let log_entry = serde_json::json!({
                        "file": file_path.display().to_string(),
                        "rule_ids": file_requests.iter().map(|r| &r.rule_id).collect::<Vec<_>>(),
                        "prompt": prompt_text,
                        "response": r.output,
                        "success": r.success,
                        "elapsed_secs": elapsed.as_secs_f64(),
                    });
                    let log_file = dir.join(format!("goose-fix-{:03}.json", i + 1));
                    let _ = std::fs::write(
                        &log_file,
                        serde_json::to_string_pretty(&log_entry).unwrap_or_default(),
                    );
                }

                results.push(r);
            }
            Err(e) => {
                failed += 1;
                eprintln!(
                    "\r         goose: ERROR ({:.1}s) — {}",
                    elapsed.as_secs_f64(),
                    e
                );
                results.push(GooseFixResult {
                    file_path: file_path.clone(),
                    rule_id: file_requests
                        .iter()
                        .map(|r| r.rule_id.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    success: false,
                    output: format!("Error: {}", e),
                });
            }
        }
        eprintln!();
    }

    let total_elapsed = pipeline_start.elapsed();
    eprintln!(
        "  Goose complete: {} succeeded, {} failed ({:.0}s total, {:.1}s avg per file)",
        succeeded,
        failed,
        total_elapsed.as_secs_f64(),
        total_elapsed.as_secs_f64() / total_files.max(1) as f64,
    );

    results
}

// ── Prompt construction ───────────────────────────────────────────────────

fn build_prompt(request: &LlmFixRequest) -> String {
    let code_context = request
        .code_snip
        .as_deref()
        .unwrap_or("(no code snippet available)");

    format!(
        r#"You are applying a PatternFly v5 to v6 migration fix.

File: {file_path}
Line: {line}

Migration rule [{rule_id}]:
{message}

Code context around line {line}:
```
{code_context}
```

Instructions:
1. Read the file at {file_path}
2. Apply ONLY the change described by the migration rule at or near line {line}
3. Make the minimum edit necessary — do not change unrelated code
4. Write the fixed file

Do not explain what you're doing. Just read the file, make the edit, and write it."#,
        file_path = request.file_path.display(),
        line = request.line,
        rule_id = request.rule_id,
        message = request.message,
        code_context = code_context,
    )
}

fn build_batch_prompt(file_path: &PathBuf, requests: &[&LlmFixRequest]) -> String {
    let mut fixes = String::new();
    for (i, req) in requests.iter().enumerate() {
        let code_context = req.code_snip.as_deref().unwrap_or("(no snippet)");

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
            num = i + 1,
            line = req.line,
            rule_id = req.rule_id,
            message = req.message,
            code_context = code_context,
        ));
    }

    format!(
        r#"You are applying PatternFly v5 to v6 migration fixes to a single file.

File: {file_path}

Apply ALL of the following {count} fixes to this file:
{fixes}
Instructions:
1. Read the file at {file_path}
2. Apply ALL {count} fixes described above
3. Make the minimum edits necessary — do not change unrelated code
4. Write the fixed file once with all changes applied

Do not explain what you're doing. Just read the file, make the edits, and write it."#,
        file_path = file_path.display(),
        count = requests.len(),
        fixes = fixes,
    )
}
