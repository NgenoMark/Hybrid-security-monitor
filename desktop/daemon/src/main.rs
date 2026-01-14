use axum::{routing::post, Json, Router};
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize)]
struct BrowserEvent {
    event_type: String,
    url: String,
    ts_ms: u64,
}

async fn ingest(Json(payload): Json<BrowserEvent>) -> &'static str {
    info!("Event: {:?}\n", payload);
    "ok"
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new().route("/ingest", post(ingest));

    let addr = "127.0.0.1:8787";
    info!("Desktop daemon listening on http://{}/ingest", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
