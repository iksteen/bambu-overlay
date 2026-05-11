use std::{convert::Infallible, net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use async_stream::stream;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::get,
    Json, Router,
};
use minijinja::{context, Environment};
use serde_json::json;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::{
    assets,
    bambu::{BambuClient, MQTT_HOST, MQTT_PORT},
    mqtt::{initial_device_ids, supervise, MqttRuntime},
    overlay::{error_payload, SnapshotService},
    video::{mjpeg_content_type, VideoConfig, VideoRuntime},
};

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8765;
pub const DEFAULT_REFRESH_SECONDS: f64 = 10.0;
pub const DEFAULT_TASK_LIMIT: usize = 20;

#[derive(Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub task_limit: usize,
    pub refresh_seconds: f64,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub no_mqtt: bool,
    pub video: VideoConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_owned(),
            port: DEFAULT_PORT,
            task_limit: DEFAULT_TASK_LIMIT,
            refresh_seconds: DEFAULT_REFRESH_SECONDS,
            mqtt_host: MQTT_HOST.to_owned(),
            mqtt_port: MQTT_PORT,
            no_mqtt: false,
            video: VideoConfig::default(),
        }
    }
}

#[derive(Clone)]
struct AppState {
    snapshot: SnapshotService,
    mqtt: MqttRuntime,
    video: VideoRuntime,
}

pub async fn serve(client: BambuClient, access_token: String, config: ServerConfig) -> Result<()> {
    let mqtt = MqttRuntime::new();
    let snapshot = SnapshotService::new(
        client.clone(),
        access_token.clone(),
        config.task_limit,
        Duration::from_secs_f64(config.refresh_seconds),
        mqtt.clone(),
    );
    let video = VideoRuntime::new(client.clone(), access_token.clone(), config.video.clone())?;

    if config.no_mqtt {
        mqtt.set_disabled("disabled by --no-mqtt").await;
    } else {
        let device_ids = match initial_device_ids(&client, &access_token).await {
            Ok(ids) => ids,
            Err(error) => {
                warn!(%error, "could not bootstrap MQTT device ids from HTTP");
                Vec::new()
            }
        };
        let mqtt_user_id = match client.user_preference(&access_token).await {
            Ok(preference) => preference.mqtt_user_id(),
            Err(error) => {
                warn!(%error, "could not fetch MQTT user id from user preference");
                None
            }
        };

        match (mqtt_user_id, device_ids.is_empty()) {
            (Some(user_id), false) => {
                let mqtt_status = mqtt.clone();
                let supervisor = tokio::spawn(supervise(
                    mqtt.clone(),
                    config.mqtt_host.clone(),
                    config.mqtt_port,
                    user_id,
                    access_token,
                    device_ids,
                ));
                tokio::spawn(async move {
                    match supervisor.await {
                        Ok(()) => {
                            warn!("MQTT supervisor exited unexpectedly");
                            mqtt_status
                                .set_error("MQTT supervisor exited unexpectedly")
                                .await;
                        }
                        Err(error) => {
                            error!(%error, "MQTT supervisor task failed");
                            mqtt_status
                                .set_error(format!("MQTT supervisor task failed: {error}"))
                                .await;
                        }
                    }
                });
            }
            (_, true) => mqtt.set_disabled("no device ids returned by HTTP").await,
            (None, false) => mqtt.set_disabled("could not derive MQTT user id").await,
        }
    }

    let state = AppState {
        snapshot,
        mqtt,
        video,
    };
    let app = Router::new()
        .route("/", get(horizontal_overlay))
        .route("/overlay", get(horizontal_overlay))
        .route("/vertical", get(vertical_overlay))
        .route("/api/current-print", get(current_print))
        .route("/api/current-print/events", get(current_print_events))
        .route("/api/video.mjpeg", get(video_mjpeg))
        .route("/static/{file}", get(static_asset))
        .with_state(state);

    let address: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .with_context(|| format!("invalid bind address {}:{}", config.host, config.port))?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind {address}"))?;
    info!(%address, "serving Bambu overlay");
    axum::serve(listener, app)
        .await
        .context("HTTP server failed")
}

async fn horizontal_overlay() -> Result<Html<String>, Response> {
    render_overlay("horizontal").map(Html).map_err(render_error)
}

async fn vertical_overlay() -> Result<Html<String>, Response> {
    render_overlay("vertical").map(Html).map_err(render_error)
}

async fn current_print(State(state): State<AppState>) -> Response {
    match state.snapshot.payload(true).await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => {
            let payload = error_payload(error.to_string(), state.mqtt.status().await);
            (StatusCode::BAD_GATEWAY, Json(payload)).into_response()
        }
    }
}

async fn current_print_events(
    State(state): State<AppState>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let mut changes = state.mqtt.subscribe();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let stream = stream! {
        yield Ok(current_print_event(&state).await);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                received = changes.recv() => {
                    if received.is_err() {
                        changes = state.mqtt.subscribe();
                    }
                }
            }
            yield Ok(current_print_event(&state).await);
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn current_print_event(state: &AppState) -> Event {
    let payload = match state.snapshot.payload(false).await {
        Ok(payload) => serde_json::to_string(&payload),
        Err(error) => {
            let payload = error_payload(error.to_string(), state.mqtt.status().await);
            serde_json::to_string(&payload)
        }
    }
    .unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()}).to_string());

    Event::default().event("current-print").data(payload)
}

async fn video_mjpeg(State(state): State<AppState>) -> Response {
    let subscription = match state.video.subscribe().await {
        Ok(subscription) => subscription,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                error.to_string(),
            )
                .into_response();
        }
    };

    let stream = stream! {
        let mut subscription = subscription;
        loop {
            match subscription.recv().await {
                Ok(part) => yield Ok::<bytes::Bytes, Infallible>(part),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "MJPEG video client lagged behind");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    (
        [
            (header::CONTENT_TYPE, mjpeg_content_type()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
            (header::PRAGMA, "no-cache".to_owned()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn static_asset(Path(file): Path<String>) -> Response {
    match file.as_str() {
        "common.css" => asset_response("text/css; charset=utf-8", assets::COMMON_CSS),
        "horizontal.css" => asset_response("text/css; charset=utf-8", assets::HORIZONTAL_CSS),
        "vertical.css" => asset_response("text/css; charset=utf-8", assets::VERTICAL_CSS),
        "overlay.js" => asset_response("application/javascript; charset=utf-8", assets::OVERLAY_JS),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn render_overlay(view_mode: &str) -> Result<String> {
    let mut env = Environment::new();
    env.add_template("overlay.html", assets::OVERLAY_HTML)?;
    let template = env.get_template("overlay.html")?;
    let view_mode = if view_mode == "vertical" {
        "vertical"
    } else {
        "horizontal"
    };
    let config_json = serde_json::to_string(&json!({"eventsUrl": "/api/current-print/events"}))?;
    Ok(template.render(context! {
        view_mode => view_mode,
        config_json => config_json,
    })?)
}

fn asset_response(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

fn render_error(error: anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        error.to_string(),
    )
        .into_response()
}
