//! The HTTP probe routes.

use super::super::serve::{live, AppState};
use axum::{extract::State, Json};
use codeweave_rust::manager::Application;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn live_omits_sensitive_runtime_metadata() {
    let state = AppState {
        manager: Arc::new(Application::default()),
        server: serde_json::from_value(json!({
            "authMode": "bearer",
            "idleTimeoutMs": 5000
        }))
        .unwrap(),
        token: Some(Arc::new(b"secret".to_vec())),
        instance_id: Arc::from("test-instance"),
    };

    let Json(payload) = live(State(state)).await;

    assert_eq!(payload["ok"], true);
    assert_eq!(payload["idleTimeoutMs"], 5000);
    for field in ["auth", "workspace", "build"] {
        assert!(payload.get(field).is_none(), "unexpected {field} field");
    }
    assert!(!payload.to_string().contains("private"));
    assert!(!payload.to_string().contains("secret"));
}
