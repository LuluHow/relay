use anyhow::Result;
use chrono::Utc;

use crate::parser::{ConversationTurn, ParsedSession};

/// Detect messages that are relay handoff context (injected by auto-handoff)
fn is_relay_handoff(content: &str) -> bool {
    let end = content.floor_char_boundary(content.len().min(200));
    let start = &content[..end];
    start.contains("auto-handoff by relay")
        || start.contains("# Handoff —")
        || start.starts_with("Context from a previous session")
}

/// Get the first real user message (skip relay handoff injections)
fn first_real_user_message(messages: &[ConversationTurn]) -> Option<&ConversationTurn> {
    messages.iter().find(|m| !is_relay_handoff(&m.content))
}

/// Get the last real user message (skip relay handoff injections)
fn last_real_user_message(messages: &[ConversationTurn]) -> Option<&ConversationTurn> {
    messages
        .iter()
        .rev()
        .find(|m| !is_relay_handoff(&m.content))
}

/// Generate a structured handoff markdown from a parsed session
pub fn generate(session: &ParsedSession) -> Result<String> {
    let now = Utc::now().format("%Y-%m-%d %H:%M UTC");
    // Extract initial goal (first real user message, skip relay handoff injections)
    let goal = first_real_user_message(&session.user_messages)
        .map(|m| truncate(&m.content, 500))
        .unwrap_or_else(|| "(no initial prompt found)".into());

    // Extract last user message (current focus, skip handoff injections)
    let last_real = last_real_user_message(&session.user_messages);
    let first_real = first_real_user_message(&session.user_messages);
    let current_focus = match (last_real, first_real) {
        (Some(last), Some(first)) if !std::ptr::eq(last, first) => truncate(&last.content, 300),
        _ => String::new(),
    };

    // Last assistant message (current state)
    let last_assistant = session
        .assistant_messages
        .last()
        .map(|m| truncate(&m.content, 500))
        .unwrap_or_else(|| "(no assistant response)".into());

    // Recent tool activity (last 10)
    let recent_tools: Vec<String> = session
        .tool_uses
        .iter()
        .rev()
        .take(10)
        .rev()
        .map(|t| {
            if t.input_summary.is_empty() {
                format!("- {}", t.name)
            } else {
                format!("- {} `{}`", t.name, truncate(&t.input_summary, 60))
            }
        })
        .collect();

    // Files touched (deduplicated, cwd-relative if possible)
    let cwd = &session.cwd;
    let files: Vec<String> = session
        .files_touched
        .iter()
        .map(|f| {
            if !cwd.is_empty() && f.starts_with(cwd) {
                f.strip_prefix(cwd)
                    .unwrap_or(f)
                    .trim_start_matches('/')
                    .to_string()
            } else {
                f.clone()
            }
        })
        .collect();

    let mut md = String::new();

    // Header
    md.push_str(&format!("# Handoff — {}\n\n", session.project_name));
    md.push_str(&format!("**Date:** {now}\n"));
    if !session.git_branch.is_empty() && session.git_branch != "HEAD" {
        md.push_str(&format!("**Branch:** `{}`\n", session.git_branch));
    }
    md.push_str(&format!("**Turns:** {}\n\n", session.turn_count));

    // Goal
    md.push_str("## Goal\n\n");
    md.push_str(&goal);
    md.push_str("\n\n");

    // Current focus (if different from goal)
    if !current_focus.is_empty() {
        md.push_str("## Current focus\n\n");
        md.push_str(&current_focus);
        md.push_str("\n\n");
    }

    // Last state
    md.push_str("## Last assistant state\n\n");
    md.push_str(&last_assistant);
    md.push_str("\n\n");

    // Recent activity
    if !recent_tools.is_empty() {
        md.push_str("## Recent tool activity\n\n");
        for t in &recent_tools {
            md.push_str(t);
            md.push('\n');
        }
        md.push('\n');
    }

    // Files touched
    if !files.is_empty() {
        md.push_str("## Files touched\n\n");
        for f in &files {
            md.push_str(&format!("- `{f}`\n"));
        }
        md.push('\n');
    }

    // Restore instructions
    md.push_str("---\n\n");
    md.push_str("*To restore this context, run:*\n");
    md.push_str(&format!(
        "```\nrelay restore {} | pbcopy\n```\n",
        &session.session_id[..8]
    ));

    Ok(md)
}

/// Generate a handoff from the parsed session context.
/// Delegates to the structured `generate()` — no external LLM call.
pub fn generate_summary(session: &ParsedSession) -> Result<String> {
    generate(session)
}

fn truncate(s: &str, max: usize) -> String {
    // Collapse whitespace and take first `max` chars
    let cleaned: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.len() <= max {
        cleaned
    } else {
        let end = cleaned.floor_char_boundary(max);
        format!("{}...", &cleaned[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ConversationTurn, ParsedSession, ToolUse};

    fn mock_session() -> ParsedSession {
        ParsedSession {
            session_id: "abc12345-test".to_string(),
            project_name: "test-project".to_string(),
            model: "claude-sonnet-4-5-20241022".to_string(),
            git_branch: "main".to_string(),
            cwd: "/tmp/test".to_string(),
            version: "1.0.0".to_string(),
            user_messages: vec![ConversationTurn {
                content: "Build a REST API".to_string(),
                timestamp: Some("2025-01-01T00:00:00Z".to_string()),
            }],
            assistant_messages: vec![ConversationTurn {
                content: "I'll create the API endpoints.".to_string(),
                timestamp: Some("2025-01-01T00:01:00Z".to_string()),
            }],
            tool_uses: vec![ToolUse {
                name: "Write".to_string(),
                input_summary: "/tmp/test/src/main.rs".to_string(),
                timestamp: None,
            }],
            total_input_tokens: 5000,
            total_output_tokens: 2000,
            total_cache_read: 1000,
            total_cache_create: 500,
            current_context_tokens: 6000,
            context_history: vec![3000, 6000],
            compaction_count: 0,
            turn_count: 5,
            first_timestamp: Some("2025-01-01T00:00:00Z".to_string()),
            last_timestamp: Some("2025-01-01T00:10:00Z".to_string()),
            age_secs: 600,
            files_touched: vec!["/tmp/test/src/main.rs".to_string()],
        }
    }

    #[test]
    fn test_generate_handoff_contains_key_sections() {
        let session = mock_session();
        let result = generate(&session).unwrap();
        assert!(result.contains("# Handoff — test-project"));
        assert!(result.contains("## Goal"));
        assert!(result.contains("Build a REST API"));
        assert!(result.contains("## Last assistant state"));
        assert!(result.contains("## Recent tool activity"));
        assert!(result.contains("## Files touched"));
        assert!(result.contains("src/main.rs"));
    }

    #[test]
    fn test_generate_handoff_includes_branch() {
        let session = mock_session();
        let result = generate(&session).unwrap();
        assert!(result.contains("**Branch:** `main`"));
    }

    #[test]
    fn test_generate_handoff_no_branch_if_head() {
        let mut session = mock_session();
        session.git_branch = "HEAD".to_string();
        let result = generate(&session).unwrap();
        assert!(!result.contains("**Branch:**"));
    }

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello world", 100), "hello world");
    }

    #[test]
    fn test_truncate_long() {
        let long = "a ".repeat(100);
        let result = truncate(&long, 10);
        assert!(result.ends_with("..."));
        // 10 chars + "..." = 13 max
        assert!(result.len() <= 14);
    }

    #[test]
    fn test_truncate_collapses_whitespace() {
        assert_eq!(truncate("hello   world", 100), "hello world");
    }

    #[test]
    fn test_is_relay_handoff_detects_auto_handoff() {
        assert!(is_relay_handoff(
            "Context from a previous session (auto-handoff by relay, context at 80%). Resume:"
        ));
    }

    #[test]
    fn test_is_relay_handoff_detects_header() {
        assert!(is_relay_handoff(
            "# Handoff — myproject\n\n**Date:** 2025-01-01"
        ));
    }

    #[test]
    fn test_is_relay_handoff_rejects_normal() {
        assert!(!is_relay_handoff("Build a REST API for the dashboard"));
    }

    #[test]
    fn test_first_real_user_message_skips_handoff() {
        let messages = vec![
            ConversationTurn {
                content: "Context from a previous session (auto-handoff by relay, ctx)."
                    .to_string(),
                timestamp: None,
            },
            ConversationTurn {
                content: "Build a REST API".to_string(),
                timestamp: None,
            },
        ];
        let first = first_real_user_message(&messages).unwrap();
        assert_eq!(first.content, "Build a REST API");
    }
}
