pub mod auth;
pub mod routes;
pub mod state;
pub mod ws;

use std::time::Duration;

use anyhow::Result;
use axum::Router;

use crate::config::Config;
use state::AppState;

/// Start the relay API server.
pub async fn serve(config: Config, bind: String) -> Result<()> {
    let app_state = AppState::new(config);

    // Background task: refresh session data every 3 seconds
    let poll_state = app_state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        loop {
            interval.tick().await;
            poll_state.refresh().await;
        }
    });

    let app = Router::new();
    // TODO: attach routes in next task
    let _ = app_state; // will be used as Router state once routes are wired

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("relay API listening on http://{bind}");
    axum::serve(listener, app).await?;

    Ok(())
}
