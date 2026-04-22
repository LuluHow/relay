mod config;
mod git;
mod handoff;
mod hooks;
mod parser;
mod session;
mod statusline;
mod storage;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "relay",
    version,
    about = "Monitor Claude Code sessions and generate handoffs"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Save a handoff for the current (or specified) session
    Save {
        /// Session ID (defaults to most recent active session)
        #[arg(short, long)]
        session: Option<String>,
        /// Project path filter
        #[arg(short, long)]
        project: Option<String>,
    },
    /// List saved handoffs
    List,
    /// Restore a handoff (print to stdout for piping into claude)
    Restore {
        /// Handoff ID (from list)
        id: String,
    },
    /// Show active sessions with token usage
    Status,
    /// Create default config file at ~/.relay/config.toml
    Init,
    /// Remove all traces of relay (config, hooks, shell wrapper, binary)
    Uninstall,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let command = match cli.command {
        Some(cmd) => cmd,
        None => return tui::run(),
    };

    match command {
        Commands::Save { session, project } => {
            let sessions = session::discover_sessions()?;
            let target = session::pick_session(&sessions, session.as_deref(), project.as_deref())?;
            let parsed = parser::parse_session(&target)?;
            let handoff = handoff::generate_summary(&parsed)?;
            let id = storage::save(&handoff, &target)?;
            println!("Handoff saved: {id}");
        }
        Commands::List => {
            storage::list()?;
        }
        Commands::Restore { id } => {
            storage::restore(&id)?;
        }
        Commands::Status => {
            let sessions = session::discover_sessions()?;
            session::print_status(&sessions)?;
        }
        Commands::Init => {
            let path = config::ensure_default()?;
            println!("Config ready: {}", path.display());
            println!("Shell wrapper: ~/.relay/claude-wrapper.sh");
            println!();

            // Detect user's shell for correct instructions
            let shell = std::env::var("SHELL").unwrap_or_default();
            let rc_file = if shell.contains("zsh") {
                ".zshrc"
            } else if shell.contains("bash") {
                ".bashrc"
            } else if shell.contains("fish") {
                ".config/fish/config.fish"
            } else {
                ".bashrc"
            };

            if shell.contains("fish") {
                println!("Note: the shell wrapper requires bash or zsh.");
                println!("If you use fish, add a bash/zsh alias or run claude from bash.");
            } else {
                println!("Add this to your ~/{rc_file}:");
                println!("  source ~/.relay/claude-wrapper.sh");
            }
            println!();
            println!("Then set auto_handoff = true in ~/.relay/config.toml");
        }
        Commands::Uninstall => {
            config::uninstall()?;
        }
    }

    Ok(())
}
