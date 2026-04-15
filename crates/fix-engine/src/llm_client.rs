//! LLM client for AI-assisted fix generation.
//!
//! Sends code snippets + violation messages to an LLM endpoint and parses
//! the response into text edits.

use anyhow::Result;
use frontend_core::fix::{
    FixConfidence, FixSource, LlmFixRequest, PlannedFix, PlannedOpenAiRequest, TextEdit,
};
use serde::{Deserialize, Serialize};

use crate::context::FixContext;

const DEFAULT_OPENAI_MODEL: &str = "gpt-4";

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

/// Build a non-mutating preview of the OpenAI-compatible request for one fix.
pub fn build_openai_plan_request(
    request: &LlmFixRequest,
    ctx: &dyn FixContext,
) -> Result<PlannedOpenAiRequest> {
    let source = match &request.source {
        Some(source) => source.clone(),
        None => std::fs::read_to_string(&request.file_path)?,
    };
    let system_prompt = ctx.llm_system_prompt();
    let user_prompt = build_user_prompt(request, &source);

    let chat_request = ChatRequest {
        model: DEFAULT_OPENAI_MODEL.to_string(),
        messages: vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_prompt.clone(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_prompt.clone(),
            },
        ],
        temperature: 0.0,
    };

    Ok(PlannedOpenAiRequest {
        rule_id: request.rule_id.clone(),
        file_path: request.file_path.clone(),
        line: request.line,
        model: DEFAULT_OPENAI_MODEL.to_string(),
        temperature: 0.0,
        system_prompt,
        user_prompt,
        request_json: serde_json::to_value(&chat_request)?,
    })
}

/// Send an LLM fix request and return planned fixes.
pub async fn request_llm_fix(
    endpoint: &str,
    request: &LlmFixRequest,
    ctx: &dyn FixContext,
) -> Result<Vec<PlannedFix>> {
    let preview = build_openai_plan_request(request, ctx)?;

    let chat_request = ChatRequest {
        model: preview.model.clone(),
        messages: vec![
            ChatMessage {
                role: "system".to_string(),
                content: preview.system_prompt,
            },
            ChatMessage {
                role: "user".to_string(),
                content: preview.user_prompt,
            },
        ],
        temperature: preview.temperature,
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

fn build_user_prompt(request: &LlmFixRequest, source: &str) -> String {
    format!(
        "File: {}\nLine: {}\n\nMigration rule: {}\n\nMessage: {}\n\nFull file source:\n```\n{}\n```",
        request.file_path.display(),
        request.line,
        request.rule_id,
        request.message,
        source,
    )
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
                    replace_all: false,
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
    use crate::context::GenericFixContext;
    use std::fs;

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

    #[test]
    fn test_parse_llm_response_empty_input() {
        let edits = parse_llm_fix_response("", "rule-1");
        assert!(edits.is_empty());
    }

    #[test]
    fn test_parse_llm_response_no_fix_blocks() {
        let response = "Here is some explanation text without any fix blocks.";
        let edits = parse_llm_fix_response(response, "rule-1");
        assert!(edits.is_empty());
    }

    #[test]
    fn test_parse_llm_response_non_fix_code_blocks_ignored() {
        let response = r#"
```typescript
const x = 1;
```

```javascript
console.log("hello");
```
"#;
        let edits = parse_llm_fix_response(response, "rule-1");
        assert!(edits.is_empty());
    }

    #[test]
    fn test_parse_llm_response_single_fix() {
        let response = r#"
```fix
LINE:1
OLD:import { Chip } from '@patternfly/react-core';
NEW:import { Label } from '@patternfly/react-core';
```
"#;
        let edits = parse_llm_fix_response(response, "rename-rule");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].line, 1);
        assert_eq!(
            edits[0].old_text,
            "import { Chip } from '@patternfly/react-core';"
        );
        assert_eq!(
            edits[0].new_text,
            "import { Label } from '@patternfly/react-core';"
        );
        assert_eq!(edits[0].rule_id, "rename-rule");
    }

    #[test]
    fn test_parse_llm_response_incomplete_fix_block_skipped() {
        // Missing NEW: line — should not produce an edit
        let response = r#"
```fix
LINE:5
OLD:something
```
"#;
        let edits = parse_llm_fix_response(response, "rule-1");
        assert!(edits.is_empty());
    }

    #[test]
    fn test_parse_llm_response_missing_line_skipped() {
        // Has OLD and NEW but no LINE: — should not produce an edit
        let response = r#"
```fix
OLD:old text
NEW:new text
```
"#;
        let edits = parse_llm_fix_response(response, "rule-1");
        assert!(edits.is_empty());
    }

    #[test]
    fn test_parse_llm_response_whitespace_tolerance() {
        let response = r#"
```fix
LINE:  42
OLD:  <Chip />
NEW:  <Label />
```
"#;
        let edits = parse_llm_fix_response(response, "rule-1");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].line, 42);
        // OLD/NEW preserve the text after the prefix
        assert_eq!(edits[0].old_text, "  <Chip />");
        assert_eq!(edits[0].new_text, "  <Label />");
    }

    #[test]
    fn test_parse_llm_response_new_can_be_empty() {
        // Removing a line entirely — NEW is empty
        let response = r#"
```fix
LINE:10
OLD:  isHidden={true}
NEW:
```
"#;
        let edits = parse_llm_fix_response(response, "rule-1");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old_text, "  isHidden={true}");
        assert_eq!(edits[0].new_text, "");
    }

    #[test]
    fn test_build_openai_plan_request() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("App.tsx");
        fs::write(&file_path, "const x = <Modal />;\n").unwrap();

        let request = LlmFixRequest {
            rule_id: "rule-1".to_string(),
            file_uri: format!("file://{}", file_path.display()),
            file_path: file_path.clone(),
            line: 1,
            message: "Restructure Modal".to_string(),
            code_snip: None,
            source: None,
            labels: vec!["family=Modal".to_string()],
        };

        let preview = build_openai_plan_request(&request, &GenericFixContext).unwrap();
        assert_eq!(preview.rule_id, "rule-1");
        assert_eq!(preview.file_path, file_path);
        assert_eq!(preview.model, "gpt-4");
        assert!(preview.system_prompt.contains("code migration"));
        assert!(preview.user_prompt.contains("Restructure Modal"));
        assert!(preview.request_json.get("messages").is_some());
    }
}
