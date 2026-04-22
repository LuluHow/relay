pub mod auth;
pub mod routes;
pub mod state;
pub mod ws;

use std::time::Duration;

use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;

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

    let app = Router::new()
        .route("/api/health", get(routes::health))
        .route("/api/sessions", get(routes::list_sessions))
        .route("/api/sessions/{id}", get(routes::get_session))
        .route("/api/sessions/{id}/handoff", post(routes::create_handoff))
        .route("/api/handoffs", get(routes::list_handoffs))
        .route("/api/handoffs/{id}", get(routes::get_handoff))
        .route("/api/config", get(routes::get_config))
        .layer(CorsLayer::permissive())
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("relay API listening on http://{bind}");
    axum::serve(listener, app).await?;

    Ok(())
}
