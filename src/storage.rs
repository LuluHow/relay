use anyhow::{bail, Context, Result};
use chrono::Utc;
use colored::Colorize;
use std::path::PathBuf;

use crate::session::SessionInfo;

fn handoffs_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("No home directory")?
        .join(".relay")
        .join("handoffs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Save a handoff markdown and return its ID
pub fn save(content: &str, session: &SessionInfo) -> Result<String> {
    let dir = handoffs_dir()?;
    let ts = Utc::now().format("%Y%m%d-%H%M%S");
    let short_id = &session.session_id[..8.min(session.session_id.len())];
    let filename = format!("{ts}_{}.md", short_id);
    let path = dir.join(&filename);

    std::fs::write(&path, content)?;

    // ID = filename without extension
    let id = filename.trim_end_matches(".md").to_string();
    Ok(id)
}

/// List all saved handoffs
pub fn list() -> Result<()> {
    let dir = handoffs_dir()?;
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map_or(false, |ext| ext == "md")
        })
        .collect();

    if entries.is_empty() {
        println!("No handoffs saved yet. Use `relay save` to create one.");
        return Ok(());
    }

    // Sort by name (which includes timestamp, so chronological)
    entries.sort_by_key(|e| e.file_name());

    println!("{}", " HANDOFFS".bold());
    println!();

    for entry in entries.iter().rev() {
        let name = entry.file_name().to_string_lossy().to_string();
        let id = name.trim_end_matches(".md");

        // Read first 3 lines for context
        let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let preview: String = content
            .lines()
            .find(|l| l.starts_with("**Context:**"))
            .unwrap_or("")
            .to_string();

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);

        println!(
            "  {} {} {}",
            id.cyan(),
            format!("({:.1}kb)", size as f64 / 1024.0).dimmed(),
            preview.dimmed(),
        );
    }

    println!();
    println!(
        "  {} relay restore <id>",
        "Restore:".dimmed()
    );

    Ok(())
}

/// Restore a handoff: print its content to stdout
pub fn restore(id: &str) -> Result<()> {
    let dir = handoffs_dir()?;

    // Find matching file (prefix match)
    let matching: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with(id) || name.trim_end_matches(".md").ends_with(id)
        })
        .collect();

    if matching.is_empty() {
        bail!("No handoff matching '{id}'. Run `relay list` to see available handoffs.");
    }

    if matching.len() > 1 {
        bail!(
            "Multiple handoffs match '{id}'. Be more specific:\n{}",
            matching
                .iter()
                .map(|e| format!("  - {}", e.file_name().to_string_lossy().trim_end_matches(".md")))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    let content = std::fs::read_to_string(matching[0].path())?;

    // Output as a restore prompt
    println!("Here is context from a previous session. Resume from where we left off:\n");
    println!("---\n");
    print!("{content}");

    Ok(())
}
