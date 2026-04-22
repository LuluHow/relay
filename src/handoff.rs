use anyhow::Result;
use chrono::Utc;

use crate::parser::{ConversationTurn, ParsedSession};

fn infer_window(model: &str, observed: u64) -> u64 {
    let m = model.to_lowercase();
    if m.contains("[1m]") || m.contains("opus") || observed > 180_000 {
        1_000_000
    } else {
        200_000
    }
}

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
    messages.iter().rev().find(|m| !is_relay_handoff(&m.content))
}

/// Generate a structured handoff markdown from a parsed session
pub fn generate(session: &ParsedSession) -> Result<String> {
    let now = Utc::now().format("%Y-%m-%d %H:%M UTC");
    let window = infer_window(&session.model, session.current_context_tokens);
    let pct = if window > 0 {
        session.current_context_tokens as f64 / window as f64 * 100.0
    } else {
        0.0
    };

    let total_tokens = session.total_input_tokens
        + session.total_output_tokens
        + session.total_cache_read
        + session.total_cache_create;

    // Extract initial goal (first real user message, skip relay handoff injections)
    let goal = first_real_user_message(&session.user_messages)
        .map(|m| truncate(&m.content, 500))
        .unwrap_or_else(|| "(no initial prompt found)".into());

    // Extract last user message (current focus, skip handoff injections)
    let last_real = last_real_user_message(&session.user_messages);
    let first_real = first_real_user_message(&session.user_messages);
    let current_focus = match (last_real, first_real) {
        (Some(last), Some(first)) if !std::ptr::eq(last, first) => {
            truncate(&last.content, 300)
        }
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
    md.push_str(&format!("**Session:** `{}`\n", session.session_id));
    if !session.git_branch.is_empty() && session.git_branch != "HEAD" {
        md.push_str(&format!("**Branch:** `{}`\n", session.git_branch));
    }
    md.push_str(&format!("**Model:** `{}`\n", session.model));
    md.push_str(&format!("**CWD:** `{}`\n", session.cwd));
    md.push_str(&format!(
        "**Context:** {:.0}% ({} / {} tokens)\n",
        pct,
        format_tokens(session.current_context_tokens),
        format_tokens(window)
    ));
    md.push_str(&format!(
        "**Total tokens used:** {} ({} turns, {} compactions)\n\n",
        format_tokens(total_tokens),
        session.turn_count,
        session.compaction_count
    ));

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
        format!("{}...", &cleaned[..max])
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}
