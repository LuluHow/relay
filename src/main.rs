mod api;
mod config;
mod git;
mod handoff;
mod hooks;
mod notify;
mod orchestrator;
mod parser;
mod prompt_runner;
mod session;
mod statusline;
mod storage;
mod tui;
mod util;

use anyhow::{Context, Result};
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
    /// Send a test notification to configured webhooks (Discord, Slack)
    TestNotify,
    /// Remove all traces of relay (config, hooks, shell wrapper, binary)
    Uninstall,
    /// Start the web API server
    Serve {
        /// Port to listen on
        #[arg(long, default_value_t = 4747)]
        port: u16,
        /// Address to bind to
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Bearer token for API authentication (overrides config file)
        #[arg(long)]
        token: Option<String>,
    },
    /// Print instructions for exposing the relay API via a tunnel
    Tunnel {
        /// Port the relay server is listening on
        #[arg(long, default_value_t = 4747)]
        port: u16,
    },
    /// Orchestrate multiple Claude sessions from a plan file
    Orchestrate {
        /// Path to the plan TOML file
        plan: Option<String>,
        /// Interactively create a new plan file
        #[arg(long)]
        create_plan: bool,
        /// Output path for --create-plan (default: plan.toml)
        #[arg(short, long, default_value = "plan.toml")]
        output: String,
    },
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
                println!("Add this to your ~/{rc_file}:");
                println!("  source ~/.relay/claude-wrapper.fish");
            } else {
                println!("Add this to your ~/{rc_file}:");
                println!("  source ~/.relay/claude-wrapper.sh");
            }
            println!();
            println!("Then set auto_handoff = true in ~/.relay/config.toml");
        }
        Commands::TestNotify => {
            let cfg = config::load()?;
            let results = notify::test(&cfg);
            if results.is_empty() {
                println!("No webhooks configured in ~/.relay/config.toml");
                println!("Set discord_webhook or slack_webhook and try again.");
            } else {
                for (name, result) in results {
                    match result {
                        Ok(()) => println!("  \u{2713} {name}: sent"),
                        Err(e) => println!("  \u{2717} {name}: {e}"),
                    }
                }
            }
        }
        Commands::Uninstall => {
            config::uninstall()?;
        }
        Commands::Serve { port, bind, token } => {
            let mut cfg = config::load()?;
            if let Some(t) = token {
                cfg.api_token = Some(t);
            }
            // CLI args override config, but fall back to config values
            // when the user didn't explicitly pass --bind or --port
            let effective_bind = if bind == "127.0.0.1" {
                &cfg.api_bind
            } else {
                &bind
            };
            let effective_port = if port == 4747 { cfg.api_port } else { port };
            let addr = format!("{effective_bind}:{effective_port}");
            if cfg.api_token.is_some() {
                println!("relay API listening on http://{addr} (auth: token required)");
            } else {
                eprintln!("⚠ No API token set — API is unauthenticated. Use --token or set api_token in config.");
                println!("relay API listening on http://{addr}");
            }
            tokio::runtime::Runtime::new()?.block_on(api::serve(cfg, addr))?;
        }
        Commands::Tunnel { port } => {
            let cloudflared_found = std::process::Command::new("cloudflared")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            if cloudflared_found {
                println!("Run this command to expose your relay API:");
                println!();
                println!("  cloudflared tunnel --url http://127.0.0.1:{port}");
                println!();
                println!("cloudflared will print a public https://... URL you can share.");
            } else {
                println!("cloudflared not found. Install it to create a secure tunnel:");
                println!();
                println!("  macOS:  brew install cloudflared");
                println!("  Linux:  curl -L https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 \\");
                println!("            -o cloudflared && chmod +x cloudflared && sudo mv cloudflared /usr/local/bin/");
                println!();
                println!("Then run:");
                println!("  cloudflared tunnel --url http://127.0.0.1:{port}");
                println!();
                println!("Alternative: Tailscale (https://tailscale.com) lets you access relay");
                println!("  from any device on your Tailnet without opening ports.");
            }
        }
        Commands::Orchestrate {
            plan,
            create_plan,
            output,
        } => {
            if create_plan {
                let output_path = std::path::Path::new(&output);
                return orchestrator::create_plan_interactive(output_path);
            }

            let plan_file = plan.ok_or_else(|| {
                anyhow::anyhow!(
                    "Missing plan file. Usage:\n  relay orchestrate <plan.toml>\n  relay orchestrate --create-plan"
                )
            })?;
            let plan_path = std::path::Path::new(&plan_file);
            let loaded = orchestrator::load_plan(plan_path)?;
            let project_root =
                std::env::current_dir().context("Cannot determine current directory")?;
            if !git::is_git_repo(&project_root.to_string_lossy()) {
                anyhow::bail!(
                    "Not a git repository: {}. Orchestration requires git for worktrees.",
                    project_root.display()
                );
            }
            println!(
                "  ▸ plan '{}' — {} tasks",
                loaded.plan.name,
                loaded.tasks.len()
            );
            if loaded.plan.skip_permissions {
                println!("  ⚠ skip_permissions is ON — tasks run without permission checks");
            }
            return tui::run_orchestrate(loaded, project_root);
        }
    }

    Ok(())
}
