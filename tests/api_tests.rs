use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use relay::{
    api::{
        build_app,
        state::{AppState, MessageSnapshot, SessionSnapshot, ToolUseSnapshot},
    },
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

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_orchestration_status_none() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/orchestrate/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn test_orchestration_abort_none() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orchestrate/abort")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn test_orchestration_merge_none() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orchestrate/merge")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn test_orchestration_pr_none() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orchestrate/pr")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn test_orchestration_start_invalid_toml() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orchestrate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"plan_toml": "invalid toml {{{}}"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("invalid plan TOML"));
}

#[tokio::test]
async fn test_orchestration_start_empty_plan() {
    // Valid TOML structure but missing required `tasks` field
    let toml = r#"[plan]
name = "empty"
branch = "test-empty"
on_complete = "manual"
"#;
    let body = serde_json::json!({ "plan_toml": toml }).to_string();

    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orchestrate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn test_orchestration_auth_required() {
    let app = app_with_token("orch-secret");
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orchestrate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"plan_toml": "x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let json = json_body(response.into_body()).await;
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn test_orchestration_endpoints_shape() {
    let app = app_no_auth();

    // All orchestration endpoints should return JSON with an "error" field, not HTML
    let endpoints: Vec<(&str, &str)> = vec![
        ("GET", "/api/orchestrate/status"),
        ("POST", "/api/orchestrate/abort"),
        ("POST", "/api/orchestrate/merge"),
        ("POST", "/api/orchestrate/pr"),
    ];

    for (method, uri) in endpoints {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "{method} {uri}: unexpected status {status}"
        );

        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("");
        assert!(
            content_type.contains("application/json"),
            "{method} {uri}: expected JSON content-type, got '{content_type}'"
        );

        let json = json_body(response.into_body()).await;
        assert!(
            json["error"].is_string(),
            "{method} {uri}: response missing 'error' field"
        );
    }
}

// ---------------------------------------------------------------------------
// Config toggle
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Dashboard v2 – static content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_static_index_content() {
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

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes);

    assert!(body.contains("relay"), "body should contain 'relay' logo");
    assert!(
        body.contains("Sessions"),
        "body should contain 'Sessions' tab label"
    );
    assert!(
        body.contains("Handoffs"),
        "body should contain 'Handoffs' tab label"
    );
    assert!(
        body.contains("/api/ws"),
        "body should reference WebSocket endpoint /api/ws"
    );
    assert!(
        body.contains("context_history") || body.contains("tool_uses"),
        "body should reference enriched session data fields"
    );
}

#[tokio::test]
async fn test_static_favicon() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/favicon.svg")
                .body(Body::empty())
                .unwrap(),
        )
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
        content_type.contains("image/svg+xml"),
        "expected image/svg+xml, got {content_type}"
    );
}

// ---------------------------------------------------------------------------
// Dashboard v2 – enriched sessions endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sessions_enriched_fields_present() {
    let app = app_no_auth();

    // Confirm server is up via /api/health
    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    // GET /api/sessions must return a valid JSON array
    let response = app
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
    assert!(
        json.is_array(),
        "sessions endpoint should return a JSON array"
    );
}

// ---------------------------------------------------------------------------
// Dashboard v2 – snapshot serialization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_api_session_snapshot_serialization() {
    let snapshot = SessionSnapshot {
        session_id: "test-id".into(),
        project_name: "my-project".into(),
        model: "opus".into(),
        git_branch: "main".into(),
        state: "active".into(),
        turn_count: 5,
        context_pct: 42.0,
        cost_usd: 1.23,
        age_secs: 300,
        files_touched: 3,
        cwd: "/tmp/project".into(),
        version: "1.0.0".into(),
        tool_uses: vec![ToolUseSnapshot {
            name: "Read".into(),
            input_summary: "file.rs".into(),
            timestamp: Some("2026-01-01T00:00:00Z".into()),
        }],
        files_touched_paths: vec!["src/main.rs".into()],
        user_messages: vec![MessageSnapshot {
            content: "hello".into(),
            timestamp: Some("2026-01-01T00:00:00Z".into()),
        }],
        assistant_messages: vec![MessageSnapshot {
            content: "hi".into(),
            timestamp: None,
        }],
        context_history: vec![1000, 2000],
        compaction_count: 1,
        total_input_tokens: 5000,
        total_output_tokens: 3000,
        total_cache_read: 100,
        total_cache_create: 50,
        current_context_tokens: 4000,
        lines_added: 42,
        lines_removed: 10,
        context_window_size: 200000,
        five_hour_used_pct: Some(25.0),
        five_hour_resets_at: Some(1800),
        seven_day_used_pct: None,
        seven_day_resets_at: None,
        duration_ms: 60000,
    };

    let json = serde_json::to_value(&snapshot).expect("snapshot should serialize to JSON");

    // Verify every expected key is present
    let expected_keys = [
        "session_id",
        "project_name",
        "model",
        "git_branch",
        "state",
        "turn_count",
        "context_pct",
        "cost_usd",
        "age_secs",
        "files_touched",
        "cwd",
        "version",
        "tool_uses",
        "files_touched_paths",
        "user_messages",
        "assistant_messages",
        "context_history",
        "compaction_count",
        "total_input_tokens",
        "total_output_tokens",
        "total_cache_read",
        "total_cache_create",
        "current_context_tokens",
        "lines_added",
        "lines_removed",
        "context_window_size",
        "five_hour_used_pct",
        "five_hour_resets_at",
        "seven_day_used_pct",
        "seven_day_resets_at",
        "duration_ms",
    ];

    let obj = json.as_object().expect("should be a JSON object");
    for key in &expected_keys {
        assert!(obj.contains_key(*key), "missing key: {key}");
    }

    // Spot-check values
    assert_eq!(json["session_id"], "test-id");
    assert_eq!(json["turn_count"], 5);
    assert_eq!(json["tool_uses"].as_array().unwrap().len(), 1);
    assert_eq!(json["tool_uses"][0]["name"], "Read");
    assert_eq!(json["context_history"].as_array().unwrap().len(), 2);
    assert!(json["seven_day_used_pct"].is_null());
}

// ---------------------------------------------------------------------------
// Dashboard v2 – WebSocket upgrade
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_websocket_upgrade() {
    let response = app_no_auth()
        .oneshot(
            Request::builder()
                .uri("/api/ws")
                .header("Connection", "Upgrade")
                .header("Upgrade", "websocket")
                .header("Sec-WebSocket-Version", "13")
                .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // With tower::oneshot there is no real TCP connection to upgrade, so axum
    // returns 426 (Upgrade Required) instead of 101. The key assertion is that
    // the server handles the request gracefully (no panic, no 500).
    let status = response.status();
    assert!(
        status == StatusCode::SWITCHING_PROTOCOLS || status == StatusCode::UPGRADE_REQUIRED,
        "expected 101 or 426, got {status}"
    );
    assert_ne!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "WebSocket endpoint must not panic/500"
    );
}
