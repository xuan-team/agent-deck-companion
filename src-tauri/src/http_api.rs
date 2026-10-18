use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use tauri::AppHandle;
use tower_http::cors::{Any, CorsLayer};

use crate::hook_integration;
use crate::monitor;
use crate::state::{emit_state, AgentStatus, AppRuntime, DeviceEvent};

#[derive(Clone)]
pub struct HttpContext {
    pub state: Arc<AppRuntime>,
    pub app: AppHandle,
}

#[derive(Deserialize)]
struct KeyRequest {
    key: String,
}

#[derive(Deserialize)]
struct StatusRequest {
    agent_id: String,
    status: AgentStatus,
}

#[derive(Debug, Deserialize)]
struct UsageRequest {
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    remaining_tokens: Option<u64>,
    remaining_percent: Option<u8>,
    remaining_5h_percent: Option<u8>,
    remaining_weekly_percent: Option<u8>,
    reset_seconds: Option<u32>,
    reset_time: Option<String>,
    reset_5h_time: Option<String>,
    reset_weekly_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EncoderRequest {
    delta: i32,
}

/// Body of a `dsh-deck-bridge` push. Only used for logging: the state itself is
/// re-read by the same sampler the poller uses, so there is exactly one
/// normalisation path.
#[derive(Debug, Deserialize)]
struct DshPushRequest {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    sessions: Vec<String>,
}

#[derive(Deserialize)]
struct TabRequest {
    tab: crate::session_tabs::Tab,
}
#[derive(Deserialize)]
struct TabOrderRequest {
    order: Vec<crate::session_tabs::Tab>,
}
#[derive(Deserialize)]
struct ThemeRequest {
    theme: crate::state::UiTheme,
}
#[derive(Deserialize)]
struct LocaleRequest {
    locale: crate::state::UiLocale,
}
async fn http_ui_locale(
    State(ctx): State<HttpContext>,
    Json(request): Json<LocaleRequest>,
) -> Result<Json<()>, (StatusCode, String)> {
    crate::commands::update_ui_locale(request.locale, &ctx.app, &ctx.state)
        .map(Json)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}
async fn http_ui_theme(
    State(ctx): State<HttpContext>,
    Json(request): Json<ThemeRequest>,
) -> Result<Json<()>, (StatusCode, String)> {
    crate::commands::update_ui_theme(request.theme, &ctx.app, &ctx.state)
        .map(Json)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}
async fn http_panel_preferences(
    State(ctx): State<HttpContext>,
) -> Json<crate::session_window::SessionPanelPreferences> {
    Json(crate::commands::panel_preferences(&ctx.state))
}
async fn http_agent_activity(State(ctx): State<HttpContext>) -> Json<serde_json::Value> {
    let snapshot = ctx.state.read();
    Json(
        serde_json::json!({"agents":snapshot.desktop_sessions.iter().filter(|row|row.identity.is_some() && row.status!=AgentStatus::Offline).map(|row| {
        let mut value=serde_json::to_value(&row.activity).unwrap_or_default();
        value["id"]=serde_json::json!(row.identity);
        value["agent"]=serde_json::json!(row.agent_id);
        value["runtime"]=serde_json::json!(row.source);
        value["status"]=serde_json::json!(row.status);
        value
    }).collect::<Vec<_>>()}),
    )
}
async fn http_panel_tab(
    State(ctx): State<HttpContext>,
    Json(request): Json<TabRequest>,
) -> Json<()> {
    crate::commands::update_panel_tab(request.tab, &ctx.app, &ctx.state);
    Json(())
}
async fn http_panel_tab_order(
    State(ctx): State<HttpContext>,
    Json(request): Json<TabOrderRequest>,
) -> Result<Json<()>, (StatusCode, String)> {
    crate::commands::update_panel_tab_order(request.order, &ctx.app, &ctx.state)
        .map(|_| Json(()))
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

async fn http_state(State(ctx): State<HttpContext>) -> Json<crate::state::AppSnapshot> {
    Json(ctx.state.read())
}

async fn http_desktop_notifications(
    State(ctx): State<HttpContext>,
) -> Json<crate::desktop_notifications::NotificationView> {
    ctx.state.desktop_notifications.refresh_permission();
    Json(ctx.state.desktop_notifications.view())
}

async fn http_notification_audio(
    State(ctx): State<HttpContext>,
) -> Json<crate::notification_audio::AudioView> {
    Json(ctx.state.notification_audio.view())
}

async fn http_hook_integrations(
    State(ctx): State<HttpContext>,
) -> Result<Json<Vec<hook_integration::HookIntegrationState>>, (StatusCode, String)> {
    // Same list the Tauri command returns, DSH plugin entry included.
    let resource_dir = crate::commands::resource_dir(&ctx.app);
    hook_integration::integration_states_with_dsh(resource_dir.as_deref())
        .map(Json)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))
}

async fn http_hook(
    State(ctx): State<HttpContext>,
    AxumPath(agent_id): AxumPath<String>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<crate::state::AppSnapshot>, (StatusCode, String)> {
    let token = headers
        .get("x-agent-deck-token")
        .and_then(|value| value.to_str().ok());
    if !hook_integration::token_matches(token) {
        return Err((StatusCode::UNAUTHORIZED, "invalid hook token".into()));
    }
    crate::hooks::process_hook_event(
        &agent_id,
        payload,
        crate::hooks::hook_transport(&headers),
        &ctx.app,
        &ctx.state,
    )
    .map(Json)
    .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

async fn http_mock_hook(
    State(ctx): State<HttpContext>,
    AxumPath(agent_id): AxumPath<String>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<crate::state::AppSnapshot>, (StatusCode, String)> {
    // Same transport derivation as the real bridge, so a mocked event can be a
    // locatable row instead of only an unattributed one.
    crate::hooks::process_hook_event(
        &agent_id,
        payload,
        crate::hooks::hook_transport(&headers),
        &ctx.app,
        &ctx.state,
    )
    .map(Json)
    .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

/// The `dsh-deck-bridge` host plugin telling us to look at DSH now.
///
/// The push carries no authoritative state: it is a trigger, and the same
/// `poll_dsh_status` sampler the 5-second poller uses reads and normalises the
/// sessions. That keeps one owner for DSH semantics and means a malformed or
/// stale push can never invent a session.
async fn http_dsh_push(
    State(ctx): State<HttpContext>,
    headers: HeaderMap,
    Json(body): Json<DshPushRequest>,
) -> Result<Json<()>, (StatusCode, String)> {
    let token = headers
        .get("x-agent-deck-token")
        .and_then(|value| value.to_str().ok());
    if !hook_integration::token_matches(token) {
        return Err((StatusCode::UNAUTHORIZED, "invalid push token".into()));
    }
    if !ctx.state.agent_enabled("dsh") {
        // Monitoring is off; accept and ignore rather than letting the plugin
        // retry against a switch the user deliberately flipped.
        return Ok(Json(()));
    }
    ctx.state.note_dsh_push();
    eprintln!(
        "[dsh] push reason={} sessions={}",
        body.reason.as_deref().unwrap_or("unspecified"),
        body.sessions
            .iter()
            .filter(|session| crate::dsh::is_valid_session_id(session))
            .count()
    );
    if monitor::poll_dsh_status(&ctx.state) {
        emit_state(&ctx.app, &ctx.state);
    }
    Ok(Json(()))
}

async fn http_key(
    State(ctx): State<HttpContext>,
    Json(body): Json<KeyRequest>,
) -> Json<crate::state::AppSnapshot> {
    Json(crate::process_event(
        DeviceEvent {
            navigation_stamp: None,
            kind: "key_down".into(),
            key: Some(body.key),
            delta: None,
        },
        &ctx.app,
        &ctx.state,
    ))
}

async fn http_encoder(
    State(ctx): State<HttpContext>,
    Json(body): Json<EncoderRequest>,
) -> Json<crate::state::AppSnapshot> {
    Json(crate::process_event(
        DeviceEvent {
            navigation_stamp: None,
            kind: "encoder".into(),
            key: None,
            delta: Some(body.delta),
        },
        &ctx.app,
        &ctx.state,
    ))
}

async fn http_status(
    State(ctx): State<HttpContext>,
    Json(body): Json<StatusRequest>,
) -> Result<Json<crate::state::AppSnapshot>, (StatusCode, String)> {
    if !ctx
        .state
        .read()
        .agents
        .iter()
        .any(|agent| agent.id == body.agent_id)
    {
        return Err((StatusCode::NOT_FOUND, "unknown agent".into()));
    }
    ctx.state
        .overrides
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "lock poisoned".into()))?
        .insert(body.agent_id, body.status);
    monitor::reconcile_processes_once(&ctx.state);
    emit_state(&ctx.app, &ctx.state);
    Ok(Json(ctx.state.read()))
}

async fn http_reset(State(ctx): State<HttpContext>) -> Json<crate::state::AppSnapshot> {
    ctx.state
        .overrides
        .lock()
        .expect("overrides poisoned")
        .clear();
    monitor::reconcile_processes_once(&ctx.state);
    emit_state(&ctx.app, &ctx.state);
    Json(ctx.state.read())
}

async fn http_mock_usage(
    State(ctx): State<HttpContext>,
    Json(body): Json<UsageRequest>,
) -> Json<crate::state::AppSnapshot> {
    {
        let mut snapshot = ctx.state.snapshot.write().expect("snapshot poisoned");
        snapshot.chatgpt_usage = crate::state::ChatGptUsage {
            model: body.model,
            input_tokens: body.input_tokens,
            output_tokens: body.output_tokens,
            remaining_tokens: body.remaining_tokens,
            remaining_percent: body.remaining_percent,
            remaining_5h_percent: body.remaining_5h_percent.or(body.remaining_percent),
            remaining_weekly_percent: body.remaining_weekly_percent,
            reset_seconds: body.reset_seconds,
            reset_time: body.reset_time,
            reset_5h_time: body.reset_5h_time,
            reset_weekly_time: body.reset_weekly_time,
            updated_at_ms: crate::state::now_ms(),
            snapshot_at_ms: crate::state::now_ms(),
            source: "mock".into(),
        };
        snapshot.updated_at_ms = crate::state::now_ms();
    }
    crate::vendor_hid::request_deck_push(&ctx.state);
    emit_state(&ctx.app, &ctx.state);
    Json(ctx.state.read())
}

pub fn start_http_api(app: AppHandle, state: Arc<AppRuntime>) {
    tauri::async_runtime::spawn(async move {
        let context = HttpContext { state, app };
        let router = Router::new()
            .route("/health", get(|| async { "ok" }))
            // Tiny restart detection for local event plugins. No state
            // projection, process scan, or terminal read is performed here.
            .route("/api/instance", get(|| async { Json(std::process::id()) }))
            .route("/api/state", get(http_state))
            .route("/api/v1/agents", get(http_agent_activity))
            .route(
                "/api/session-panel/preferences",
                get(http_panel_preferences),
            )
            .route("/api/session-panel/tab", post(http_panel_tab))
            .route("/api/session-panel/tab-order", post(http_panel_tab_order))
            .route("/api/ui-theme", post(http_ui_theme))
            .route("/api/ui-locale", post(http_ui_locale))
            .route(
                "/api/desktop-notifications",
                get(http_desktop_notifications),
            )
            .route("/api/notification-audio", get(http_notification_audio))
            .route("/api/hook-integrations", get(http_hook_integrations))
            .route("/api/hooks/{agent_id}", post(http_hook))
            // The DSH deck plugin's "look now" trigger. Token-gated like the
            // hook bridge, because any local process can reach loopback.
            .route("/api/dsh/push", post(http_dsh_push))
            .route("/api/hardware/key", post(http_key))
            .route("/api/hardware/encoder", post(http_encoder))
            .route("/api/mock/hooks/{agent_id}", post(http_mock_hook))
            .route("/api/mock/status", post(http_status))
            .route("/api/mock/reset", post(http_reset))
            .route("/api/mock/chatgpt-usage", post(http_mock_usage))
            .layer(
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_headers(Any)
                    .allow_methods(Any),
            )
            .with_state(context);
        let port = 34115;
        #[cfg(debug_assertions)]
        let port = std::env::var("AGENT_DECK_QA_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|p| *p > 0)
            .unwrap_or(port);
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => {
                if let Err(error) = axum::serve(listener, router).await {
                    eprintln!("mock API stopped: {error}");
                }
            }
            Err(error) => eprintln!("mock API unavailable on port {port}: {error}"),
        }
    });
}
