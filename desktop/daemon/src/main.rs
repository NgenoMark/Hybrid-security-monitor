use axum::{routing::post, Json, Router};
use jsonschema::JSONSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::OnceLock;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{error, info};

#[derive(Debug, Deserialize, Serialize)]
struct BrowserEventV1 {
    // Allows forward compatibility
    schema_version: String, // e.g. "1.0"

    event_type: String,     // e.g. "tab_active", "worker_created", "wasm_instantiate"
    ts_ms: u64,

    // Optional fields to support correlation later
    url: Option<String>,
    origin: Option<String>, // e.g. "example.com"
    tab_id: Option<u32>,

    // Flexible event-specific payload
    #[serde(default)]
    details: Value, // any JSON object
}

const LOG_DIR: &str = "logs";
const LOG_FILE: &str = "logs/events.jsonl";
const EVENT_SCHEMA_JSON: &str = include_str!("../../../shared/event.schema.json");

fn event_schema() -> &'static JSONSchema {
    static SCHEMA: OnceLock<JSONSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let schema_json: Value =
            serde_json::from_str(EVENT_SCHEMA_JSON).expect("Invalid event schema JSON");
        JSONSchema::options()
            .compile(&schema_json)
            .expect("Failed to compile event schema")
    })
}

async fn ensure_log_path() {
    if let Err(e) = fs::create_dir_all(LOG_DIR).await {
        error!("Failed to create log directory '{}': {}", LOG_DIR, e);
    }
}

async fn append_jsonl(line: &str) {
    // Open in append mode (create if missing)
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to open log file '{}': {}", LOG_FILE, e);
            return;
        }
    };

    if let Err(e) = file.write_all(line.as_bytes()).await {
        error!("Failed to write to log file '{}': {}", LOG_FILE, e);
        return;
    }
    if let Err(e) = file.write_all(b"\n").await {
        error!("Failed to write newline to log file '{}': {}", LOG_FILE, e);
    }
}

async fn ingest(Json(payload): Json<BrowserEventV1>) -> Json<Value> {
    let payload_value = match serde_json::to_value(&payload) {
        Ok(value) => value,
        Err(e) => {
            error!("Failed to convert payload to JSON value: {}", e);
            return Json(json!({ "ok": false, "error": "serialize_failed" }));
        }
    };

    if let Err(errors) = event_schema().validate(&payload_value) {
        for err in errors {
            error!("Event schema validation error: {}", err);
        }
    }

    // Pretty log to console (good for dev)
    match serde_json::to_string_pretty(&payload) {
        Ok(pretty) => info!("Incoming event:\n{}", pretty),
        Err(e) => info!("Incoming event (failed to pretty-print): {:?} ({})", payload, e),
    }

    // Write JSONL (one compact JSON per line)
    match serde_json::to_string(&payload) {
        Ok(line) => append_jsonl(&line).await,
        Err(e) => error!("Failed to serialize event for JSONL: {}", e),
    }

    // Respond with a simple ack (helps extension debugging)
    Json(json!({
        "ok": true,
        "received_event_type": payload.event_type,
        "ts_ms": payload.ts_ms
    }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    ensure_log_path().await;

    let app = Router::new().route("/ingest", post(ingest));

    let addr = "127.0.0.1:8787";
    info!("Desktop daemon listening on http://{}/ingest", addr);
    info!("Writing events to {}", LOG_FILE);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
