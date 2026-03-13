//! LLM client for AI-assisted fix generation.
//!
//! Sends code snippets + violation messages to an LLM endpoint and parses
//! the response into text edits.

use anyhow::Result;
use frontend_core::fix::*;
use serde::{Deserialize, Serialize};

/// An OpenAI-compatible chat completion request.
#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

/// An OpenAI-compatible chat completion response.
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: String,
}

/// Send an LLM fix request and return planned fixes.
pub async fn request_llm_fix(
    endpoint: &str,
    request: &LlmFixRequest,
) -> Result<Vec<PlannedFix>> {
    // Read the source file for full context
    let source = std::fs::read_to_string(&request.file_path)?;

    let system_prompt = "You are a PatternFly v5 to v6 migration assistant. \
        Given a code snippet and a migration message, output ONLY the corrected \
        code for the affected lines. Output in this exact format:\n\n\
        ```fix\n\
        LINE:<line_number>\n\
        OLD:<exact old text on that line>\n\
        NEW:<replacement text>\n\
        ```\n\n\
        You may output multiple fix blocks. Do not include any explanation outside \
        the fix blocks. Only output fixes for lines that need to change.";

    let user_prompt = format!(
        "File: {}\nLine: {}\n\nMigration rule: {}\n\nMessage: {}\n\nFull file source:\n```\n{}\n```",
        request.file_path.display(),
        request.line,
        request.rule_id,
        request.message,
        source,
    );

    let chat_request = ChatRequest {
        model: "gpt-4".to_string(),
        messages: vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_prompt,
            },
        ],
        temperature: 0.0,
    };

    let client = reqwest::Client::new();
    let response = client
        .post(endpoint)
        .json(&chat_request)
        .send()
        .await?
        .json::<ChatResponse>()
        .await?;

    let content = response
        .choices
        .first()
        .map(|c| c.message.content.as_str())
        .unwrap_or("");

    let edits = parse_llm_fix_response(content, &request.rule_id);

    if edits.is_empty() {
        return Ok(Vec::new());
    }

    Ok(vec![PlannedFix {
        edits,
        confidence: FixConfidence::Medium,
        source: FixSource::Llm,
        rule_id: request.rule_id.clone(),
        file_uri: request.file_uri.clone(),
        line: request.line,
        description: format!("LLM-generated fix for {}", request.rule_id),
    }])
}

/// Parse the LLM response format into text edits.
fn parse_llm_fix_response(content: &str, rule_id: &str) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    let mut in_fix_block = false;
    let mut current_line: Option<u32> = None;
    let mut current_old: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed == "```fix" {
            in_fix_block = true;
            continue;
        }
        if trimmed == "```" && in_fix_block {
            in_fix_block = false;
            continue;
        }

        if !in_fix_block {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("LINE:") {
            current_line = rest.trim().parse().ok();
        } else if let Some(rest) = trimmed.strip_prefix("OLD:") {
            current_old = Some(rest.to_string());
        } else if let Some(rest) = trimmed.strip_prefix("NEW:") {
            if let (Some(line_num), Some(old_text)) = (current_line, current_old.take()) {
                edits.push(TextEdit {
                    line: line_num,
                    old_text,
                    new_text: rest.to_string(),
                    rule_id: rule_id.to_string(),
                    description: "LLM-generated fix".to_string(),
                });
            }
            current_line = None;
        }
    }

    edits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_llm_response() {
        let response = r#"
```fix
LINE:56
OLD:<BarsIcon />
NEW:<PageToggleButton isHamburgerButton />
```

```fix
LINE:10
OLD:import { Button, BarsIcon } from '@patternfly/react-core';
NEW:import { PageToggleButton } from '@patternfly/react-core';
```
"#;
        let edits = parse_llm_fix_response(response, "test-rule");
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].line, 56);
        assert_eq!(edits[0].old_text, "<BarsIcon />");
        assert_eq!(edits[0].new_text, "<PageToggleButton isHamburgerButton />");
        assert_eq!(edits[1].line, 10);
    }
}
