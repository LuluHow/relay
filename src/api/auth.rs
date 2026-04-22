use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use subtle::ConstantTimeEq;

use crate::api::state::AppState;

pub async fn auth_middleware(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // Always pass through CORS preflight requests
    if req.method() == Method::OPTIONS {
        return next.run(req).await;
    }

    let expected_token = state.config().await.api_token;

    // No token configured → unauthenticated mode, pass through
    let Some(expected) = expected_token else {
        return next.run(req).await;
    };

    let is_ws = req.uri().path() == "/api/ws";

    // Check Authorization: Bearer <token> header
    if let Some(auth_header) = req.headers().get(axum::http::header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Some(bearer_token) = auth_str.strip_prefix("Bearer ") {
                if constant_time_eq(bearer_token, &expected) {
                    return next.run(req).await;
                }
            }
        }
    }

    // WebSocket: browsers can't set custom headers, so also accept ?token=<value>
    if is_ws {
        if let Some(query) = req.uri().query() {
            for part in query.split('&') {
                if let Some(token_val) = part.strip_prefix("token=") {
                    if constant_time_eq(token_val, &expected) {
                        return next.run(req).await;
                    }
                }
            }
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "unauthorized"})),
    )
        .into_response()
}

/// Constant-time string comparison to avoid timing attacks.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    bool::from(a.ct_eq(b))
}
