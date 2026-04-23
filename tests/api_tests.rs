use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use relay::{
    api::{build_app, state::AppState},
    config::Config,
};
use tower::ServiceExt;

fn app_no_auth() -> Router {
    build_app(AppState::new(Config::default()))
}

fn app_with_token(token: &str) -> Router {
    build_app(AppState::new(Config {
        api_token: Some(token.to_string()),
        ..Config::default()
    }))
}

async fn json_body(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_endpoint() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response.into_body()).await;
    assert_eq!(json["status"], "ok");
    assert!(json["version"].is_string());
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sessions_empty() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response.into_body()).await;
    assert!(json.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_session_not_found() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/sessions/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

// ---------------------------------------------------------------------------
// Handoffs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_handoffs_empty() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/handoffs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    // May or may not be empty depending on the local filesystem; just check shape.
    let json = json_body(response.into_body()).await;
    assert!(json.is_array());
}

#[tokio::test]
async fn test_handoff_not_found() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/handoffs/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_config_endpoint() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response.into_body()).await;
    assert!(json["threshold"].is_number());
    assert!(json["api_port"].is_number());
    assert!(json["interval"].is_number());
}

#[tokio::test]
async fn test_config_redacts_token() {
    let app = app_with_token("my-secret");
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .header("Authorization", "Bearer my-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response.into_body()).await;
    assert_eq!(json["api_token"], "***");
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_required() {
    let app = app_with_token("secret");
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn test_auth_valid_token() {
    let app = app_with_token("secret");
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_auth_invalid_token() {
    let app = app_with_token("secret");
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .header("Authorization", "Bearer wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Static files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_static_index() {
    let response = app_no_auth()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .expect("content-type header missing")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("text/html"),
        "expected text/html, got {content_type}"
    );
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cors_headers() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .header("Origin", "http://localhost:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .contains_key("access-control-allow-origin"),
        "CORS header missing"
    );
}

// ---------------------------------------------------------------------------
// Session snapshot shape (enriched fields)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_snapshot_shape() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response.into_body()).await;
    let arr = json.as_array().expect("sessions should be an array");

    // If real sessions exist, verify enriched fields are present
    if let Some(first) = arr.first() {
        assert!(first["cwd"].is_string(), "missing cwd");
        assert!(first["tool_uses"].is_array(), "missing tool_uses");
        assert!(
            first["files_touched_paths"].is_array(),
            "missing files_touched_paths"
        );
        assert!(
            first["context_history"].is_array(),
            "missing context_history"
        );
        assert!(
            first["compaction_count"].is_number(),
            "missing compaction_count"
        );
        assert!(
            first["total_input_tokens"].is_number(),
            "missing total_input_tokens"
        );
        assert!(first["lines_added"].is_number(), "missing lines_added");
        assert!(
            first["context_window_size"].is_number(),
            "missing context_window_size"
        );
    }
}

// ---------------------------------------------------------------------------
// Session detail enriched (404 shape)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_detail_enriched() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/sessions/fake-session-id-000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = json_body(response.into_body()).await;
    assert_eq!(json["error"], "session not found");
}

// ---------------------------------------------------------------------------
// Config toggle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_config_toggle_auto_handoff() {
    let state = AppState::new(Config::default());
    let app = build_app(state.clone());

    // Toggle auto_handoff ON
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/toggle")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"key":"auto_handoff","value":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Verify config reflects the change
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response.into_body()).await;
    assert_eq!(json["auto_handoff"], true);

    // Toggle auto_handoff OFF
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/toggle")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"key":"auto_handoff","value":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Verify it's now false
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = json_body(response.into_body()).await;
    assert_eq!(json["auto_handoff"], false);
}

#[tokio::test]
async fn test_config_toggle_auto_commit() {
    let state = AppState::new(Config::default());
    let app = build_app(state.clone());

    // Toggle auto_commit ON
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/toggle")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"key":"auto_commit","value":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Verify config reflects the change
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response.into_body()).await;
    assert_eq!(json["auto_commit"], true);

    // Toggle auto_commit OFF
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/toggle")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"key":"auto_commit","value":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Verify it's now false
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = json_body(response.into_body()).await;
    assert_eq!(json["auto_commit"], false);
}

#[tokio::test]
async fn test_config_toggle_invalid_key() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/toggle")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"key":"invalid_key","value":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn test_config_toggle_auth() {
    let app = app_with_token("toggle-secret");

    // Without token → 401
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/toggle")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"key":"auto_handoff","value":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // With token → 200
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/toggle")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Authorization", "Bearer toggle-secret")
                .body(Body::from(r#"{"key":"auto_handoff","value":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
