use axum::{
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/health", get(health))
        .route("/echo", post(echo))
        .route("/auth/login", post(login))
        .route("/api/user", get(user));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn echo(body: String) -> String {
    body
}

async fn login() -> Json<Value> {
    Json(json!({"access_token": "bench-token"}))
}

async fn user(headers: HeaderMap) -> Result<Json<Value>, StatusCode> {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "Bearer bench-token")
        .unwrap_or(false);
    if ok {
        Ok(Json(json!({"id": 1, "name": "bench"})))
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}
