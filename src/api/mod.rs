pub mod auth;
pub mod routes;
pub mod state;
pub mod ws;

use std::time::Duration;

use anyhow::Result;
use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use rust_embed::RustEmbed;
use tower_http::cors::CorsLayer;

use crate::config::Config;
use state::AppState;

#[derive(RustEmbed)]
#[folder = "static/"]
struct Assets;

async fn static_handler(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    serve_embedded(path)
}

fn serve_embedded(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => Response::builder()
            .header(header::CONTENT_TYPE, mime_for_path(path))
            .body(Body::from(file.data.into_owned()))
            .unwrap(),
        None => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

fn mime_for_path(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

/// Build the app router from an existing AppState (used by both `serve` and tests).
pub fn build_app(app_state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(routes::health))
        .route("/api/sessions", get(routes::list_sessions))
        .route("/api/sessions/{id}", get(routes::get_session))
        .route("/api/sessions/{id}/handoff", post(routes::create_handoff))
        .route("/api/handoffs", get(routes::list_handoffs))
        .route("/api/handoffs/{id}", get(routes::get_handoff))
        .route("/api/projects", get(routes::list_projects))
        .route("/api/config", get(routes::get_config))
        .route("/api/config/toggle", post(routes::toggle_config))
        .route("/api/orchestrate", post(routes::start_orchestration))
        .route("/api/orchestrate/status", get(routes::orchestration_status))
        .route("/api/orchestrate/abort", post(routes::abort_orchestration))
        .route("/api/orchestrate/merge", post(routes::merge_orchestration))
        .route("/api/orchestrate/pr", post(routes::create_orchestration_pr))
        .route("/api/plans/history", get(routes::list_plan_history))
        .route("/api/plans/history/{id}", get(routes::get_plan_history))
        .route("/api/ws", get(ws::handler))
        .fallback(static_handler)
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            auth::auth_middleware,
        ))
        .layer(CorsLayer::permissive())
        .with_state(app_state)
}

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

    let app = build_app(app_state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
