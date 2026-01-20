use axum::{extract::State, response::Html, routing::get, routing::post, Json, Router};
use jsonschema::JSONSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::System;
use tokio::sync::Mutex;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::time::{interval, Duration};
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
const CPU_SAMPLE_INTERVAL_SECS: u64 = 2;
const CPU_HIGH_THRESHOLD_PCT: f32 = 25.0;
const CPU_HIGH_MIN_SECS: u64 = 10;
const WORKER_RECENT_MS: u64 = 30_000;
const WASM_RECENT_MS: u64 = 30_000;
const MEDIA_RECENT_MS: u64 = 30_000;
const HIDDEN_RECENT_MS: u64 = 10_000;
const IDLE_MS: u64 = 15_000;
const ALERT_COOLDOWN_MS: u64 = 30_000;
const DASHBOARD_REFRESH_SECS: u64 = 3;
const MAX_EVENT_HISTORY: usize = 50;
const IGNORE_ORIGINS: [&str; 1] = ["127.0.0.1"];

#[derive(Default)]
struct CorrelationState {
    origins: HashMap<String, OriginState>,
    active_origin: Option<String>,
    last_alert_ms: HashMap<String, u64>,
    cpu_high_since_ms: Option<u64>,
    last_cpu_pct: f32,
    events: VecDeque<EventSummary>,
}

#[derive(Debug, Clone, Default)]
struct OriginState {
    last_visibility: Option<String>,
    last_hidden_ms: Option<u64>,
    last_worker_ms: Option<u64>,
    last_wasm_ms: Option<u64>,
    last_media_ms: Option<u64>,
    last_media_state: Option<String>,
    last_user_activity_ms: Option<u64>,
    last_seen_ms: Option<u64>,
}

struct Alert {
    origin: String,
    score: f32,
    reasons: Vec<String>,
    ts_ms: u64,
}

type SharedState = Arc<Mutex<CorrelationState>>;

#[derive(Debug, Clone, Serialize)]
struct EventSummary {
    ts_ms: u64,
    event_type: String,
    origin: Option<String>,
}

#[derive(Debug, Serialize)]
struct OriginSnapshot {
    origin: String,
    score: f32,
    reasons: Vec<String>,
    last_visibility: Option<String>,
    last_compute_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct DashboardSnapshot {
    now_ms: u64,
    cpu_pct: f32,
    cpu_high: bool,
    cpu_high_active: bool,
    active_origin: Option<String>,
    origins: Vec<OriginSnapshot>,
    last_events: Vec<EventSummary>,
}

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

async fn ingest(State(state): State<SharedState>, Json(payload): Json<BrowserEventV1>) -> Json<Value> {
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

    handle_event(&payload, &state).await;

    // Respond with a simple ack (helps extension debugging)
    Json(json!({
        "ok": true,
        "received_event_type": payload.event_type,
        "ts_ms": payload.ts_ms
    }))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn is_recent(ts_ms: u64, now_ms: u64, window_ms: u64) -> bool {
    now_ms.saturating_sub(ts_ms) <= window_ms
}

fn cpu_high(state: &CorrelationState, now_ms: u64) -> bool {
    matches!(
        state.cpu_high_since_ms,
        Some(ts_ms) if now_ms.saturating_sub(ts_ms) >= CPU_HIGH_MIN_SECS * 1000
    )
}

fn evaluate_alerts(state: &mut CorrelationState, now_ms: u64) -> Vec<Alert> {
    let mut alerts = Vec::new();
    if !cpu_high(state, now_ms) {
        return alerts;
    }

    for (origin, origin_state) in state.origins.iter() {
        let hidden_recent = origin_state
            .last_hidden_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, HIDDEN_RECENT_MS))
            .is_some();
        let worker_recent = origin_state
            .last_worker_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, WORKER_RECENT_MS))
            .is_some();
        let wasm_recent = origin_state
            .last_wasm_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, WASM_RECENT_MS))
            .is_some();
        let media_recent = origin_state
            .last_media_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, MEDIA_RECENT_MS))
            .is_some();
        let media_playing = media_recent
            && origin_state
                .last_media_state
                .as_deref()
                .map(|state| state == "playing")
                .unwrap_or(false);
        let idle = origin_state
            .last_user_activity_ms
            .map(|ts_ms| now_ms.saturating_sub(ts_ms) > IDLE_MS)
            .unwrap_or(true);

        let compute_recent = worker_recent || wasm_recent || media_playing;
        if !compute_recent {
            continue;
        }
        if !(hidden_recent || idle) {
            continue;
        }
        if !(cpu_high(state, now_ms)) {
            continue;
        }

        if let Some(last_alert_ms) = state.last_alert_ms.get(origin) {
            if is_recent(*last_alert_ms, now_ms, ALERT_COOLDOWN_MS) {
                continue;
            }
        }

        let mut reasons = Vec::new();
        if hidden_recent {
            reasons.push("tab_hidden".to_string());
        }
        if idle {
            reasons.push("idle".to_string());
        }
        if worker_recent {
            reasons.push("worker_created".to_string());
        }
        if wasm_recent {
            reasons.push("wasm_instantiate".to_string());
        }
        if media_recent {
            reasons.push("media_playing".to_string());
        }
        reasons.push("cpu_high".to_string());

        let score = 1.0;
        state.last_alert_ms.insert(origin.clone(), now_ms);

        alerts.push(Alert {
            origin: origin.clone(),
            score,
            reasons,
            ts_ms: now_ms,
        });
    }

    alerts
}

async fn log_json(value: Value) {
    match serde_json::to_string(&value) {
        Ok(line) => append_jsonl(&line).await,
        Err(e) => error!("Failed to serialize log JSON: {}", e),
    }
}

async fn emit_alert(alert: Alert) {
    let value = json!({
        "event_type": "alert_suspicious_compute",
        "ts_ms": alert.ts_ms,
        "details": {
            "origin": alert.origin,
            "score": alert.score,
            "reasons": alert.reasons
        }
    });

    log_json(value).await;
}

async fn handle_event(payload: &BrowserEventV1, state: &SharedState) {
    let mut guard = state.lock().await;
    guard.events.push_back(EventSummary {
        ts_ms: payload.ts_ms,
        event_type: payload.event_type.clone(),
        origin: payload.origin.clone(),
    });
    while guard.events.len() > MAX_EVENT_HISTORY {
        guard.events.pop_front();
    }

    let Some(origin) = payload.origin.clone() else {
        return;
    };

    if payload.event_type == "tab_active" && !IGNORE_ORIGINS.contains(&origin.as_str()) {
        guard.active_origin = Some(origin.clone());
    }

    let entry = guard.origins.entry(origin.clone()).or_default();

    match payload.event_type.as_str() {
        "tab_active" => {
            entry.last_seen_ms = Some(payload.ts_ms);
        }
        "tab_visibility" => {
            if let Some(state_value) = payload.details.get("state").and_then(|value| value.as_str()) {
                entry.last_visibility = Some(state_value.to_string());
                if state_value == "hidden" {
                    entry.last_hidden_ms = Some(payload.ts_ms);
                }
            }
            entry.last_seen_ms = Some(payload.ts_ms);
        }
        "wasm_instantiate" => {
            entry.last_wasm_ms = Some(payload.ts_ms);
            entry.last_seen_ms = Some(payload.ts_ms);
        }
        "worker_created" => {
            entry.last_worker_ms = Some(payload.ts_ms);
            entry.last_seen_ms = Some(payload.ts_ms);
        }
        "media_playing" => {
            let state = payload
                .details
                .get("state")
                .and_then(|value| value.as_str())
                .unwrap_or("playing");
            entry.last_media_state = Some(state.to_string());
            entry.last_media_ms = Some(payload.ts_ms);
            entry.last_seen_ms = Some(payload.ts_ms);
        }
        "user_activity" => {
            entry.last_user_activity_ms = Some(payload.ts_ms);
            entry.last_seen_ms = Some(payload.ts_ms);
        }
        _ => {}
    }

    let alerts = evaluate_alerts(&mut guard, payload.ts_ms);
    drop(guard);

    for alert in alerts {
        emit_alert(alert).await;
    }
}

fn chrome_cpu_pct(system: &System) -> f32 {
    system
        .processes()
        .values()
        .filter_map(|process| {
            let name = process.name().to_ascii_lowercase();
            if name.contains("chrome") || name.contains("chromium") {
                Some(process.cpu_usage())
            } else {
                None
            }
        })
        .sum()
}

async fn sample_cpu(state: &SharedState, cpu_pct: f32, ts_ms: u64) {
    {
        let mut guard = state.lock().await;
        guard.last_cpu_pct = cpu_pct;
        if cpu_pct >= CPU_HIGH_THRESHOLD_PCT {
            if guard.cpu_high_since_ms.is_none() {
                guard.cpu_high_since_ms = Some(ts_ms);
            }
        } else {
            guard.cpu_high_since_ms = None;
        }

        guard.events.push_back(EventSummary {
            ts_ms,
            event_type: "cpu_sample".to_string(),
            origin: None,
        });
        while guard.events.len() > MAX_EVENT_HISTORY {
            guard.events.pop_front();
        }

        let alerts = evaluate_alerts(&mut guard, ts_ms);
        drop(guard);

        for alert in alerts {
            emit_alert(alert).await;
        }
    }
}

async fn start_cpu_sampler(state: SharedState) {
    tokio::spawn(async move {
        let mut system = System::new_all();
        system.refresh_processes();

        let mut ticker = interval(Duration::from_secs(CPU_SAMPLE_INTERVAL_SECS));
        loop {
            ticker.tick().await;
            system.refresh_processes();
            let cpu_pct = chrome_cpu_pct(&system);
            let ts_ms = now_ms();

            log_json(json!({
                "event_type": "cpu_sample",
                "ts_ms": ts_ms,
                "process": "chrome",
                "cpu_pct": cpu_pct
            }))
            .await;

            sample_cpu(&state, cpu_pct, ts_ms).await;
        }
    });
}

fn build_snapshot(state: &CorrelationState, now_ms: u64) -> DashboardSnapshot {
    let cpu_high = cpu_high(state, now_ms);
    let mut origins = Vec::new();
    let active_origin = state.active_origin.clone();
    let cpu_high_active = cpu_high && active_origin.is_some();

    for (origin, origin_state) in state.origins.iter() {
        let mut reasons = Vec::new();
        let active = state
            .active_origin
            .as_ref()
            .map(|active_origin| active_origin == origin)
            .unwrap_or(false);
        let hidden_recent = origin_state
            .last_hidden_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, HIDDEN_RECENT_MS))
            .is_some();
        let worker_recent = origin_state
            .last_worker_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, WORKER_RECENT_MS))
            .is_some();
        let wasm_recent = origin_state
            .last_wasm_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, WASM_RECENT_MS))
            .is_some();
        let media_recent = origin_state
            .last_media_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, MEDIA_RECENT_MS))
            .is_some();
        let media_playing = media_recent
            && origin_state
                .last_media_state
                .as_deref()
                .map(|state| state == "playing")
                .unwrap_or(false);
        let media_paused = media_recent
            && origin_state
                .last_media_state
                .as_deref()
                .map(|state| state == "paused")
                .unwrap_or(false);
        let idle = origin_state
            .last_user_activity_ms
            .map(|ts_ms| now_ms.saturating_sub(ts_ms) > IDLE_MS)
            .unwrap_or(true);
        let seen_recent = origin_state
            .last_seen_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, WORKER_RECENT_MS))
            .is_some();

        let mut score = 0.0;
        if hidden_recent {
            reasons.push("tab_hidden".to_string());
            score += 0.1;
        }
        if worker_recent {
            reasons.push("worker_created".to_string());
            score += 0.4;
        }
        if wasm_recent {
            reasons.push("recent_compute_event".to_string());
            score += 0.5;
        }
        if media_playing {
            reasons.push("media_playing".to_string());
            score += 0.3;
        }
        if media_paused {
            reasons.push("media_paused".to_string());
        }
        if cpu_high {
            if active {
                reasons.push("cpu_high_active".to_string());
                score += 0.8;
            } else if worker_recent || wasm_recent || media_recent {
                reasons.push("cpu_high_global".to_string());
                score += 0.5;
            }
        }
        if idle && seen_recent && !active {
            reasons.push("idle".to_string());
        }

        let compute_recent = worker_recent || wasm_recent || media_playing;
        if hidden_recent && compute_recent && cpu_high {
            reasons.push("correlation_bonus".to_string());
            score += 0.4;
        }

        origins.push(OriginSnapshot {
            origin: origin.clone(),
            score,
            reasons,
            last_visibility: origin_state.last_visibility.clone(),
            last_compute_ms: origin_state
                .last_worker_ms
                .into_iter()
                .chain(origin_state.last_wasm_ms)
                .max(),
        });
    }

    origins.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    DashboardSnapshot {
        now_ms,
        cpu_pct: state.last_cpu_pct,
        cpu_high,
        cpu_high_active,
        active_origin,
        origins,
        last_events: state.events.iter().cloned().collect(),
    }
}

async fn dashboard_html() -> Html<String> {
    let html = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Hybrid Security Monitor</title>
    <style>
      :root {
        color-scheme: light;
        font-family: "IBM Plex Sans", "Segoe UI", sans-serif;
        background: #f7f3ef;
        color: #1b1b1b;
      }
      body {
        margin: 0;
        padding: 32px;
        background: radial-gradient(circle at top left, #f7e6d8 0%, #f7f3ef 35%, #efe9e2 100%);
      }
      h1 {
        margin: 0 0 12px;
        font-size: 28px;
      }
      .grid {
        display: grid;
        grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
        gap: 16px;
      }
      .card {
        background: #fffdfb;
        border: 1px solid #e2d5c7;
        border-radius: 12px;
        padding: 16px;
        box-shadow: 0 12px 30px -24px rgba(0, 0, 0, 0.5);
      }
      .muted {
        color: #6a5d52;
        font-size: 13px;
      }
      .list {
        margin: 8px 0 0;
        padding: 0;
        list-style: none;
      }
      .list li {
        padding: 8px 0;
        border-bottom: 1px dashed #e6d8cb;
        font-size: 14px;
      }
      .pill {
        display: inline-block;
        padding: 2px 8px;
        border-radius: 999px;
        background: #efe3d6;
        font-size: 12px;
        margin-right: 6px;
      }
      .score-high {
        color: #7a1f1f;
        font-weight: 600;
      }
      .score-low {
        color: #2a4a2a;
      }
    </style>
  </head>
  <body>
    <h1>Hybrid Security Monitor</h1>
    <p class="muted">Live view of active origins and correlation reasons.</p>
    <div class="grid">
      <div class="card">
        <h2>System</h2>
        <div id="system" class="muted">Loading...</div>
      </div>
      <div class="card">
        <h2>Origins</h2>
        <ul class="list" id="origins"></ul>
      </div>
      <div class="card">
        <h2>Last Events</h2>
        <ul class="list" id="events"></ul>
      </div>
    </div>
    <script>
      async function refresh() {
        const res = await fetch("/dashboard/data");
        const data = await res.json();
        const system = document.getElementById("system");
        const origins = document.getElementById("origins");
        const events = document.getElementById("events");

        system.textContent = `CPU: ${data.cpu_pct.toFixed(1)}% | Global High: ${data.cpu_high ? "yes" : "no"} | Active High: ${data.cpu_high_active ? "yes" : "no"} | Now: ${data.now_ms}`;
        if (data.active_origin) {
          system.textContent += ` | Active: ${data.active_origin}`;
        }

        origins.innerHTML = "";
        data.origins.forEach((origin) => {
        const li = document.createElement("li");
        const scoreClass = origin.score > 0 ? "score-high" : "score-low";
          li.innerHTML = `<span class="pill">${origin.origin}</span><span class="${scoreClass}">score ${origin.score.toFixed(1)}</span><div class="muted">${origin.reasons.join(", ") || "no reasons"}</div>`;
          origins.appendChild(li);
        });

        events.innerHTML = "";
        data.last_events.slice(-12).reverse().forEach((event) => {
          const li = document.createElement("li");
          const origin = event.origin ? ` @ ${event.origin}` : "";
          li.textContent = `${event.event_type}${origin}`;
          events.appendChild(li);
        });
      }

      refresh();
      setInterval(refresh, 2000);
    </script>
  </body>
</html>
"#;
    Html(html.to_string())
}

async fn dashboard_data(State(state): State<SharedState>) -> Json<DashboardSnapshot> {
    let now_ms = now_ms();
    let guard = state.lock().await;
    Json(build_snapshot(&guard, now_ms))
}

async fn start_terminal_dashboard(state: SharedState) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(DASHBOARD_REFRESH_SECS));
        loop {
            ticker.tick().await;
            let now_ms = now_ms();
            let guard = state.lock().await;
            let snapshot = build_snapshot(&guard, now_ms);
            drop(guard);

            println!("---- Hybrid Security Monitor ----");
            println!(
                "CPU: {:.1}% | Global High: {} | Active High: {} | Now: {}",
                snapshot.cpu_pct,
                snapshot.cpu_high,
                snapshot.cpu_high_active,
                snapshot.now_ms
            );
            if let Some(active) = snapshot.active_origin.as_ref() {
                println!("Active origin: {}", active);
            }
            if snapshot.origins.is_empty() {
                println!("No origins tracked yet.");
            } else {
                for origin in snapshot.origins.iter().take(5) {
                    println!(
                        "Origin: {} | Score: {:.1} | Reasons: {}",
                        origin.origin,
                        origin.score,
                        if origin.reasons.is_empty() {
                            "none".to_string()
                        } else {
                            origin.reasons.join(", ")
                        }
                    );
                }
            }
            println!("Last events:");
            for event in snapshot.last_events.iter().rev().take(5) {
                let origin = event.origin.clone().unwrap_or_else(|| "system".to_string());
                println!(" - {} @ {}", event.event_type, origin);
            }
        }
    });
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    ensure_log_path().await;

    let state = Arc::new(Mutex::new(CorrelationState::default()));
    start_cpu_sampler(state.clone()).await;
    start_terminal_dashboard(state.clone()).await;

    let app = Router::new()
        .route("/ingest", post(ingest))
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard/data", get(dashboard_data))
        .with_state(state);

    let addr = "127.0.0.1:8787";
    info!("Desktop daemon listening on http://{}/ingest", addr);
    info!("Writing events to {}", LOG_FILE);
    info!("Dashboard available at http://{}/dashboard", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
