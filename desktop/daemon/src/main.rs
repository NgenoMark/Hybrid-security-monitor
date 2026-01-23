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
const SNAPSHOT_LOG_FILE: &str = "logs/snapshots.jsonl";
const ALERT_LOG_FILE: &str = "logs/alerts.jsonl";
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
const SNAPSHOT_INTERVAL_SECS: u64 = 5;
const IGNORE_ORIGINS: [&str; 1] = ["127.0.0.1"];
const WEIGHT_HIDDEN: f32 = 0.1;
const WEIGHT_WORKER: f32 = 0.4;
const WEIGHT_WASM: f32 = 0.5;
const WEIGHT_MEDIA_PLAYING: f32 = 0.3;
const WEIGHT_MEDIA_PAUSED: f32 = 0.05;
const WEIGHT_IDLE: f32 = 0.1;
const WEIGHT_CPU_HIGH_ACTIVE: f32 = 0.8;
const WEIGHT_CPU_HIGH_GLOBAL: f32 = 0.5;
const WEIGHT_CORRELATION_BONUS: f32 = 0.4;
const WEIGHT_DENYLIST: f32 = 0.6;
const DEFAULT_ALERT_SCORE_THRESHOLD: f32 = 1.0;
const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Deserialize, Default)]
struct Config {
    #[serde(default)]
    allowlist: Vec<String>,
    #[serde(default)]
    denylist: Vec<String>,
    #[serde(default)]
    origin_overrides: HashMap<String, OriginOverride>,
}

#[derive(Debug, Deserialize, Default)]
struct OriginOverride {
    #[serde(default)]
    alert_score_threshold: Option<f32>,
    #[serde(default)]
    weights: Option<WeightOverrides>,
}

#[derive(Debug, Deserialize, Default)]
struct WeightOverrides {
    hidden: Option<f32>,
    worker: Option<f32>,
    wasm: Option<f32>,
    media_playing: Option<f32>,
    media_paused: Option<f32>,
    idle: Option<f32>,
    cpu_high_active: Option<f32>,
    cpu_high_global: Option<f32>,
    correlation_bonus: Option<f32>,
    denylist: Option<f32>,
}

#[derive(Debug, Clone, Copy)]
struct Weights {
    hidden: f32,
    worker: f32,
    wasm: f32,
    media_playing: f32,
    media_paused: f32,
    idle: f32,
    cpu_high_active: f32,
    cpu_high_global: f32,
    correlation_bonus: f32,
    denylist: f32,
}

struct CorrelationState {
    config: Config,
    config_last_modified: Option<SystemTime>,
    tabs: HashMap<u32, TabState>,
    active_tab_id: Option<u32>,
    snoozed: HashMap<String, u64>,
    last_alert_ms: HashMap<String, u64>,
    cpu_high_since_ms: Option<u64>,
    last_cpu_pct: f32,
    events: VecDeque<EventSummary>,
}

#[derive(Debug, Clone, Default)]
struct TabState {
    origin: Option<String>,
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

#[derive(Debug, Serialize, Clone)]
struct OriginSnapshot {
    origin: String,
    score: f32,
    reasons: Vec<String>,
    contributions: Vec<ScoreContribution>,
    last_visibility: Option<String>,
    last_compute_ms: Option<u64>,
    snoozed_until: Option<u64>,
}

#[derive(Debug, Serialize)]
struct DashboardSnapshot {
    now_ms: u64,
    cpu_pct: f32,
    cpu_high: bool,
    cpu_high_active: bool,
    active_origin: Option<String>,
    snoozed_until: HashMap<String, u64>,
    origins: Vec<OriginSnapshot>,
    last_events: Vec<EventSummary>,
}

#[derive(Debug, Serialize, Clone)]
struct ScoreContribution {
    name: String,
    weight: f32,
}

struct OriginScore {
    score: f32,
    reasons: Vec<String>,
    contributions: Vec<ScoreContribution>,
    compute_present: bool,
    hidden_present: bool,
    idle_present: bool,
    skipped: bool,
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

fn load_config() -> (Config, Option<SystemTime>) {
    match std::fs::read_to_string(CONFIG_FILE) {
        Ok(contents) => match serde_json::from_str::<Config>(&contents) {
            Ok(config) => {
                let modified = std::fs::metadata(CONFIG_FILE)
                    .ok()
                    .and_then(|meta| meta.modified().ok());
                (config, modified)
            }
            Err(e) => {
                error!("Failed to parse {}: {}", CONFIG_FILE, e);
                (Config::default(), None)
            }
        },
        Err(e) => {
            info!("No {} found ({}), using defaults.", CONFIG_FILE, e);
            (Config::default(), None)
        }
    }
}

fn matches_pattern(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let mut remainder = value;
    let mut first = true;
    for part in pattern.split('*').filter(|part| !part.is_empty()) {
        if let Some(index) = remainder.find(part) {
            if first && !pattern.starts_with('*') && index != 0 {
                return false;
            }
            remainder = &remainder[index + part.len()..];
            first = false;
        } else {
            return false;
        }
    }
    if !pattern.ends_with('*') && !remainder.is_empty() {
        return false;
    }
    true
}

fn matches_any(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|pattern| matches_pattern(pattern, value))
}

fn build_weights(origin: &str, config: &Config) -> Weights {
    let mut weights = Weights {
        hidden: WEIGHT_HIDDEN,
        worker: WEIGHT_WORKER,
        wasm: WEIGHT_WASM,
        media_playing: WEIGHT_MEDIA_PLAYING,
        media_paused: WEIGHT_MEDIA_PAUSED,
        idle: WEIGHT_IDLE,
        cpu_high_active: WEIGHT_CPU_HIGH_ACTIVE,
        cpu_high_global: WEIGHT_CPU_HIGH_GLOBAL,
        correlation_bonus: WEIGHT_CORRELATION_BONUS,
        denylist: WEIGHT_DENYLIST,
    };

    if let Some(override_entry) = config.origin_overrides.get(origin) {
        if let Some(overrides) = override_entry.weights.as_ref() {
            if let Some(value) = overrides.hidden {
                weights.hidden = value;
            }
            if let Some(value) = overrides.worker {
                weights.worker = value;
            }
            if let Some(value) = overrides.wasm {
                weights.wasm = value;
            }
            if let Some(value) = overrides.media_playing {
                weights.media_playing = value;
            }
            if let Some(value) = overrides.media_paused {
                weights.media_paused = value;
            }
            if let Some(value) = overrides.idle {
                weights.idle = value;
            }
            if let Some(value) = overrides.cpu_high_active {
                weights.cpu_high_active = value;
            }
            if let Some(value) = overrides.cpu_high_global {
                weights.cpu_high_global = value;
            }
            if let Some(value) = overrides.correlation_bonus {
                weights.correlation_bonus = value;
            }
            if let Some(value) = overrides.denylist {
                weights.denylist = value;
            }
        }
    }

    weights
}

fn refresh_config_if_needed(state: &mut CorrelationState) {
    let modified = std::fs::metadata(CONFIG_FILE)
        .ok()
        .and_then(|meta| meta.modified().ok());
    if modified.is_none() {
        return;
    }
    let needs_reload = match (state.config_last_modified, modified) {
        (Some(prev), Some(next)) => next > prev,
        (None, Some(_)) => true,
        _ => false,
    };
    if needs_reload {
        let (config, last_modified) = load_config();
        state.config = config;
        state.config_last_modified = last_modified;
        info!("Reloaded {}", CONFIG_FILE);
    }
}

async fn ensure_log_path() {
    if let Err(e) = fs::create_dir_all(LOG_DIR).await {
        error!("Failed to create log directory '{}': {}", LOG_DIR, e);
    }
}

async fn append_jsonl(line: &str) {
    append_jsonl_to(LOG_FILE, line).await;
}

async fn append_jsonl_to(path: &str, line: &str) {
    // Open in append mode (create if missing)
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to open log file '{}': {}", path, e);
            return;
        }
    };

    if let Err(e) = file.write_all(line.as_bytes()).await {
        error!("Failed to write to log file '{}': {}", LOG_FILE, e);
        return;
    }
    if let Err(e) = file.write_all(b"\n").await {
        error!("Failed to write newline to log file '{}': {}", path, e);
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

#[derive(Deserialize)]
struct SnoozeRequest {
    origin: String,
    minutes: Option<u64>,
}

async fn snooze_origin(
    State(state): State<SharedState>,
    Json(payload): Json<SnoozeRequest>,
) -> Json<Value> {
    let until_ms = match payload.minutes {
        Some(minutes) => now_ms().saturating_add(minutes.saturating_mul(60_000)),
        None => 0,
    };
    let mut guard = state.lock().await;
    if until_ms == 0 {
        guard.snoozed.remove(&payload.origin);
    } else {
        guard.snoozed.insert(payload.origin.clone(), until_ms);
    }
    Json(json!({ "ok": true, "origin": payload.origin, "snoozed_until": until_ms }))
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

fn decayed_weight(ts_ms: Option<u64>, now_ms: u64, window_ms: u64, weight: f32) -> f32 {
    let Some(ts_ms) = ts_ms else {
        return 0.0;
    };
    let age_ms = now_ms.saturating_sub(ts_ms);
    if age_ms > window_ms {
        return 0.0;
    }
    let remaining = 1.0 - (age_ms as f32 / window_ms as f32);
    weight * remaining
}

fn score_tab(
    tab_state: &TabState,
    origin: &str,
    config: &Config,
    now_ms: u64,
    cpu_high: bool,
    active: bool,
    seen_recent: bool,
) -> OriginScore {
    if matches_any(&config.allowlist, origin) {
        return OriginScore {
            score: 0.0,
            reasons: Vec::new(),
            contributions: Vec::new(),
            compute_present: false,
            hidden_present: false,
            idle_present: false,
            skipped: true,
        };
    }

    let weights = build_weights(origin, config);

    let mut score = 0.0;
    let mut reasons = Vec::new();
    let mut contributions = Vec::new();

    let hidden_contrib = decayed_weight(
        tab_state.last_hidden_ms,
        now_ms,
        HIDDEN_RECENT_MS,
        weights.hidden,
    );
    if hidden_contrib > 0.0 {
        score += hidden_contrib;
        reasons.push("tab_hidden".to_string());
        contributions.push(ScoreContribution {
            name: "tab_hidden".to_string(),
            weight: hidden_contrib,
        });
    }

    let worker_contrib = decayed_weight(
        tab_state.last_worker_ms,
        now_ms,
        WORKER_RECENT_MS,
        weights.worker,
    );
    if worker_contrib > 0.0 {
        score += worker_contrib;
        reasons.push("worker_created".to_string());
        contributions.push(ScoreContribution {
            name: "worker_created".to_string(),
            weight: worker_contrib,
        });
    }

    let wasm_contrib = decayed_weight(
        tab_state.last_wasm_ms,
        now_ms,
        WASM_RECENT_MS,
        weights.wasm,
    );
    if wasm_contrib > 0.0 {
        score += wasm_contrib;
        reasons.push("wasm_instantiate".to_string());
        contributions.push(ScoreContribution {
            name: "wasm_instantiate".to_string(),
            weight: wasm_contrib,
        });
    }

    let media_state = tab_state.last_media_state.as_deref();
    let media_playing_contrib = if media_state == Some("playing") {
        decayed_weight(
            tab_state.last_media_ms,
            now_ms,
            MEDIA_RECENT_MS,
            weights.media_playing,
        )
    } else {
        0.0
    };
    if media_playing_contrib > 0.0 {
        score += media_playing_contrib;
        reasons.push("media_playing".to_string());
        contributions.push(ScoreContribution {
            name: "media_playing".to_string(),
            weight: media_playing_contrib,
        });
    }

    let media_paused_contrib = if media_state == Some("paused") {
        decayed_weight(
            tab_state.last_media_ms,
            now_ms,
            MEDIA_RECENT_MS,
            weights.media_paused,
        )
    } else {
        0.0
    };
    if media_paused_contrib > 0.0 {
        score += media_paused_contrib;
        reasons.push("media_paused".to_string());
        contributions.push(ScoreContribution {
            name: "media_paused".to_string(),
            weight: media_paused_contrib,
        });
    }

    let idle = tab_state
        .last_user_activity_ms
        .map(|ts_ms| now_ms.saturating_sub(ts_ms) > IDLE_MS)
        .unwrap_or(true);
    let idle_contrib = if idle && seen_recent && !active {
        weights.idle
    } else {
        0.0
    };
    if idle_contrib > 0.0 {
        score += idle_contrib;
        reasons.push("idle".to_string());
        contributions.push(ScoreContribution {
            name: "idle".to_string(),
            weight: idle_contrib,
        });
    }

    let compute_present = worker_contrib > 0.0 || wasm_contrib > 0.0 || media_playing_contrib > 0.0;
    let hidden_present = hidden_contrib > 0.0;
    let idle_present = idle_contrib > 0.0;

    if cpu_high {
        if active {
            score += weights.cpu_high_active;
            reasons.push("cpu_high_active".to_string());
            contributions.push(ScoreContribution {
                name: "cpu_high_active".to_string(),
                weight: weights.cpu_high_active,
            });
        } else if compute_present {
            score += weights.cpu_high_global;
            reasons.push("cpu_high_global".to_string());
            contributions.push(ScoreContribution {
                name: "cpu_high_global".to_string(),
                weight: weights.cpu_high_global,
            });
        }
    }

    if cpu_high && compute_present && hidden_present {
        score += weights.correlation_bonus;
        reasons.push("correlation_bonus".to_string());
        contributions.push(ScoreContribution {
            name: "correlation_bonus".to_string(),
            weight: weights.correlation_bonus,
        });
    }

    if matches_any(&config.denylist, origin) {
        score += weights.denylist;
        reasons.push("denylisted".to_string());
        contributions.push(ScoreContribution {
            name: "denylisted".to_string(),
            weight: weights.denylist,
        });
    }

    OriginScore {
        score,
        reasons,
        contributions,
        compute_present,
        hidden_present,
        idle_present,
        skipped: false,
    }
}

fn evaluate_alerts(state: &mut CorrelationState, now_ms: u64) -> Vec<Alert> {
    let mut alerts = Vec::new();
    let cpu_high = cpu_high(state, now_ms);
    if !cpu_high {
        return alerts;
    }

    let mut by_origin: HashMap<String, OriginScore> = HashMap::new();
    for (tab_id, tab_state) in state.tabs.iter() {
        let origin = match tab_state.origin.as_ref() {
            Some(origin) => origin,
            None => continue,
        };
        let seen_recent = tab_state
            .last_seen_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, WORKER_RECENT_MS))
            .is_some();
        let active = state.active_tab_id == Some(*tab_id);

        let score_details =
            score_tab(tab_state, origin, &state.config, now_ms, cpu_high, active, seen_recent);
        if score_details.skipped {
            continue;
        }
        if !score_details.compute_present {
            continue;
        }
        if !(score_details.hidden_present || score_details.idle_present) {
            continue;
        }
        if state
            .snoozed
            .get(origin)
            .map(|until| *until > now_ms)
            .unwrap_or(false)
        {
            continue;
        }

        let entry = by_origin.entry(origin.clone()).or_insert_with(|| OriginScore {
            score: 0.0,
            reasons: Vec::new(),
            contributions: Vec::new(),
            compute_present: false,
            hidden_present: false,
            idle_present: false,
            skipped: false,
        });
        if score_details.score > entry.score {
            *entry = score_details;
        }
    }

    for (origin, score_details) in by_origin {
        if let Some(last_alert_ms) = state.last_alert_ms.get(&origin) {
            if is_recent(*last_alert_ms, now_ms, ALERT_COOLDOWN_MS) {
                continue;
            }
        }

        let threshold = state
            .config
            .origin_overrides
            .get(&origin)
            .and_then(|override_entry| override_entry.alert_score_threshold)
            .unwrap_or(DEFAULT_ALERT_SCORE_THRESHOLD);
        if score_details.score < threshold {
            continue;
        }

        let score = 1.0;
        state.last_alert_ms.insert(origin.clone(), now_ms);

        alerts.push(Alert {
            origin,
            score,
            reasons: score_details.reasons,
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
    log_alert_json(&alert).await;
}

async fn log_alert_json(alert: &Alert) {
    let value = json!({
        "ts_ms": alert.ts_ms,
        "origin": alert.origin,
        "score": alert.score,
        "reasons": alert.reasons
    });
    if let Ok(line) = serde_json::to_string(&value) {
        append_jsonl_to(ALERT_LOG_FILE, &line).await;
    }
}

async fn handle_event(payload: &BrowserEventV1, state: &SharedState) {
    let mut guard = state.lock().await;
    refresh_config_if_needed(&mut guard);
    guard.events.push_back(EventSummary {
        ts_ms: payload.ts_ms,
        event_type: payload.event_type.clone(),
        origin: payload.origin.clone(),
    });
    while guard.events.len() > MAX_EVENT_HISTORY {
        guard.events.pop_front();
    }

    if payload.event_type == "tab_closed" {
        if let Some(tab_id) = payload.tab_id {
            if guard.tabs.remove(&tab_id).is_some() {
                if guard.active_tab_id == Some(tab_id) {
                    guard.active_tab_id = None;
                }
            }
        }
        return;
    }

    let Some(tab_id) = payload.tab_id else {
        return;
    };

    let incoming_origin = payload.origin.clone();
    if payload.event_type == "tab_active" {
        if let Some(origin) = incoming_origin.as_ref() {
            if !matches_any(&guard.config.allowlist, origin)
                && !IGNORE_ORIGINS.contains(&origin.as_str())
            {
                guard.active_tab_id = Some(tab_id);
            } else {
                guard.active_tab_id = None;
            }
        }
    }

    let tab_state = guard.tabs.entry(tab_id).or_default();
    if let Some(origin) = incoming_origin {
        tab_state.origin = Some(origin);
    }

    match payload.event_type.as_str() {
        "tab_active" => {
            tab_state.last_seen_ms = Some(payload.ts_ms);
        }
        "tab_visibility" => {
            if let Some(state_value) = payload.details.get("state").and_then(|value| value.as_str()) {
                tab_state.last_visibility = Some(state_value.to_string());
                if state_value == "hidden" {
                    tab_state.last_hidden_ms = Some(payload.ts_ms);
                }
            }
            tab_state.last_seen_ms = Some(payload.ts_ms);
        }
        "wasm_instantiate" => {
            tab_state.last_wasm_ms = Some(payload.ts_ms);
            tab_state.last_seen_ms = Some(payload.ts_ms);
        }
        "worker_created" => {
            tab_state.last_worker_ms = Some(payload.ts_ms);
            tab_state.last_seen_ms = Some(payload.ts_ms);
        }
        "media_playing" => {
            let state = payload
                .details
                .get("state")
                .and_then(|value| value.as_str())
                .unwrap_or("playing");
            tab_state.last_media_state = Some(state.to_string());
            tab_state.last_media_ms = Some(payload.ts_ms);
            tab_state.last_seen_ms = Some(payload.ts_ms);
        }
        "user_activity" => {
            tab_state.last_user_activity_ms = Some(payload.ts_ms);
            tab_state.last_seen_ms = Some(payload.ts_ms);
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
        refresh_config_if_needed(&mut guard);
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
    let active_origin = state
        .active_tab_id
        .and_then(|tab_id| state.tabs.get(&tab_id))
        .and_then(|tab_state| tab_state.origin.clone())
        .filter(|origin| {
            !matches_any(&state.config.allowlist, origin)
                && !state
                    .snoozed
                    .get(origin)
                    .map(|until| *until > now_ms)
                    .unwrap_or(false)
        });
    let snoozed_until = state.snoozed.clone();
    let cpu_high_active = cpu_high && active_origin.is_some();

    let mut by_origin: HashMap<String, OriginSnapshot> = HashMap::new();
    for (tab_id, tab_state) in state.tabs.iter() {
        let Some(origin) = tab_state.origin.clone() else {
            continue;
        };
        let active = state.active_tab_id == Some(*tab_id);
        let seen_recent = tab_state
            .last_seen_ms
            .filter(|ts_ms| is_recent(*ts_ms, now_ms, WORKER_RECENT_MS))
            .is_some();
        let score_details =
            score_tab(tab_state, &origin, &state.config, now_ms, cpu_high, active, seen_recent);
        if score_details.skipped {
            continue;
        }

        let snapshot = OriginSnapshot {
            origin: origin.clone(),
            score: score_details.score,
            reasons: score_details.reasons,
            contributions: score_details.contributions,
            last_visibility: tab_state.last_visibility.clone(),
            last_compute_ms: tab_state
                .last_worker_ms
                .into_iter()
                .chain(tab_state.last_wasm_ms)
                .chain(tab_state.last_media_ms)
                .max(),
            snoozed_until: state.snoozed.get(&origin).copied(),
        };

        by_origin
            .entry(origin)
            .and_modify(|existing| {
                if snapshot.score > existing.score {
                    *existing = snapshot.clone();
                }
            })
            .or_insert(snapshot);
    }

    origins.extend(by_origin.into_values());
    origins.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    DashboardSnapshot {
        now_ms,
        cpu_pct: state.last_cpu_pct,
        cpu_high,
        cpu_high_active,
        active_origin,
        snoozed_until,
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
      .explain {
        margin-top: 6px;
        color: #5d5046;
        font-size: 12px;
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
      .snooze-row {
        margin-top: 6px;
        display: flex;
        gap: 6px;
        align-items: center;
      }
      .snooze-row input {
        width: 64px;
        padding: 4px 6px;
      }
      .snooze-row button {
        padding: 4px 8px;
        border: 1px solid #c9b9aa;
        background: #efe3d6;
        border-radius: 6px;
        cursor: pointer;
      }
      .snoozed {
        opacity: 0.55;
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
          const snoozed = origin.snoozed_until && origin.snoozed_until > data.now_ms;
          const explain = origin.contributions && origin.contributions.length
            ? origin.contributions.map((c) => `${c.name}=${c.weight.toFixed(2)}`).join(", ")
            : "no contributions";
          li.innerHTML = `
            <span class="pill">${origin.origin}</span>
            <span class="${scoreClass}">score ${origin.score.toFixed(2)}</span>
            <div class="muted">${origin.reasons.join(", ") || "no reasons"}</div>
            <div class="explain">Explain: ${explain}</div>
            <div class="snooze-row">
              <label class="muted">Snooze (min)</label>
              <input type="number" min="1" max="1440" value="15" />
              <button data-origin="${origin.origin}">Snooze</button>
              <button data-origin="${origin.origin}" data-unsnooze="true">Unsnooze</button>
            </div>
          `;
          if (snoozed) {
            li.classList.add("snoozed");
          }
          origins.appendChild(li);
          const button = li.querySelector("button");
          const input = li.querySelector("input");
          const unsnoozeButton = li.querySelector("button[data-unsnooze='true']");
          button.addEventListener("click", async () => {
            const minutes = Number(input.value || 15);
            await fetch("/snooze", {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({ origin: origin.origin, minutes })
            });
          });
          unsnoozeButton.addEventListener("click", async () => {
            await fetch("/snooze", {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({ origin: origin.origin })
            });
          });
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

async fn start_snapshot_logger(state: SharedState) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(SNAPSHOT_INTERVAL_SECS));
        loop {
            ticker.tick().await;
            let now_ms = now_ms();
            let guard = state.lock().await;
            let snapshot = build_snapshot(&guard, now_ms);
            drop(guard);

            if let Ok(line) = serde_json::to_string(&snapshot) {
                append_jsonl_to(SNAPSHOT_LOG_FILE, &line).await;
            }
        }
    });
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    ensure_log_path().await;

    let (config, config_last_modified) = load_config();
    let state = Arc::new(Mutex::new(CorrelationState {
        config,
        config_last_modified,
        tabs: HashMap::new(),
        active_tab_id: None,
        snoozed: HashMap::new(),
        last_alert_ms: HashMap::new(),
        cpu_high_since_ms: None,
        last_cpu_pct: 0.0,
        events: VecDeque::new(),
    }));
    start_cpu_sampler(state.clone()).await;
    start_terminal_dashboard(state.clone()).await;
    start_snapshot_logger(state.clone()).await;

    let app = Router::new()
        .route("/ingest", post(ingest))
        .route("/snooze", post(snooze_origin))
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
