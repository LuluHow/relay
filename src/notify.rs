use crate::config::Config;
use std::thread;

/// Send handoff notifications to configured webhooks (Discord, Slack).
/// Runs in background threads so the TUI is never blocked.
pub fn send_handoff(config: &Config, reason: &str, killed: bool) {
    let status = if killed {
        format!("{reason} — restarting")
    } else {
        format!("{reason} — handoff saved (could not find claude pid)")
    };

    if let Some(url) = &config.discord_webhook {
        let url = url.clone();
        let msg = status.clone();
        thread::spawn(move || {
            let _ = send_discord(&url, &msg);
        });
    }

    if let Some(url) = &config.slack_webhook {
        let url = url.clone();
        let msg = status.clone();
        thread::spawn(move || {
            let _ = send_slack(&url, &msg);
        });
    }
}

/// Send a test message to all configured webhooks. Returns results for display.
pub fn test(config: &Config) -> Vec<(&'static str, Result<(), String>)> {
    let mut results = Vec::new();

    if let Some(url) = &config.discord_webhook {
        let r = send_discord(url, "test notification").map_err(|e| e.to_string());
        results.push(("Discord", r));
    }

    if let Some(url) = &config.slack_webhook {
        let r = send_slack(url, "test notification").map_err(|e| e.to_string());
        results.push(("Slack", r));
    }

    results
}

fn is_valid_webhook_url(url: &str) -> bool {
    url.starts_with("https://discord.com/api/webhooks/")
        || url.starts_with("https://discordapp.com/api/webhooks/")
        || url.starts_with("https://hooks.slack.com/services/")
}

fn send_discord(webhook_url: &str, message: &str) -> Result<(), Box<ureq::Error>> {
    if !is_valid_webhook_url(webhook_url) {
        return Err(Box::new(ureq::Error::from(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Discord webhook URL",
        ))));
    }
    ureq::post(webhook_url)
        .set("Content-Type", "application/json")
        .send_json(serde_json::json!({
            "content": format!("**relay** — {message}")
        }))?;
    Ok(())
}

fn send_slack(webhook_url: &str, message: &str) -> Result<(), Box<ureq::Error>> {
    if !is_valid_webhook_url(webhook_url) {
        return Err(Box::new(ureq::Error::from(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Slack webhook URL",
        ))));
    }
    ureq::post(webhook_url)
        .set("Content-Type", "application/json")
        .send_json(serde_json::json!({
            "text": format!("*relay* — {message}")
        }))?;
    Ok(())
}
