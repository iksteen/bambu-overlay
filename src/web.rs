use std::{collections::HashSet, convert::Infallible, net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use async_stream::stream;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::get,
    Json, Router,
};
use minijinja::{context, Environment};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::{
    assets,
    bambu::{CloudDevice, MQTT_HOST, MQTT_PORT},
    cloud::{cloud_mqtt_startup, start_cloud_mqtt},
    devices::{resolve_devices, resolve_video_endpoints, ResolvedDevices, ResolvedVideoEndpoints},
    local::{Endpoint, LocalDeviceConfig, MqttEndpoint},
    mqtt::{start_local_supervisors, MqttRuntime},
    overlay::{error_payload, SnapshotService},
    video::{mjpeg_content_type, VideoEndpoint, VideoRuntime},
};

pub use crate::{cloud::CloudSession, devices::DeviceConfig};

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8765;

#[derive(Clone)]
pub struct ServerConfig {
    pub bind: Endpoint,
    pub cloud_mqtt: MqttEndpoint,
    pub no_cloud_enum: bool,
    pub local_devices: Vec<LocalDeviceConfig>,
    pub cloud_devices: Vec<DeviceConfig>,
    pub video_endpoints: Vec<VideoEndpoint>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: Endpoint::new(DEFAULT_HOST, DEFAULT_PORT),
            cloud_mqtt: MqttEndpoint::new(MQTT_HOST, MQTT_PORT),
            no_cloud_enum: false,
            local_devices: Vec::new(),
            cloud_devices: Vec::new(),
            video_endpoints: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct AppState {
    snapshot: SnapshotService,
    mqtt: MqttRuntime,
    video: VideoRuntime,
    devices: KnownDevices,
}

#[derive(Debug, Deserialize)]
struct DeviceQuery {
    device: Option<String>,
}

#[derive(Clone)]
struct KnownDevices {
    devices: Vec<CloudDevice>,
    local_ids: HashSet<String>,
    probed_video_ids: HashSet<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct KnownDevicesPayload {
    devices: Vec<KnownDevicePayload>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct KnownDevicePayload {
    id: Option<String>,
    name: Option<String>,
    online: Option<bool>,
    source: DeviceSource,
    paths: KnownDevicePaths,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct KnownDevicePaths {
    horizontal: Option<String>,
    vertical: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    video: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
enum DeviceSource {
    Cloud,
    Local,
}

pub async fn serve(cloud: Option<CloudSession>, config: ServerConfig) -> Result<()> {
    let mqtt = MqttRuntime::new();
    let devices = resolve_devices(
        cloud.as_ref(),
        &config.cloud_devices,
        &config.local_devices,
        config.no_cloud_enum,
    )
    .await?;
    let cloud_mqtt =
        cloud_mqtt_startup(cloud.as_ref(), &config.cloud_mqtt, &devices.cloud_mqtt_ids).await?;
    let video = resolve_video_endpoints(&config.video_endpoints, &devices).await?;
    let state = app_state(mqtt.clone(), &devices, video)?;

    start_cloud_mqtt(mqtt.clone(), cloud_mqtt);
    start_local_supervisors(mqtt, devices.local);
    serve_http(config.bind, state).await
}

async fn serve_http(bind: Endpoint, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/", get(horizontal_overlay))
        .route("/overlay", get(horizontal_overlay))
        .route("/vertical", get(vertical_overlay))
        .route("/api/devices", get(known_devices))
        .route("/api/current-print", get(current_print))
        .route("/api/current-print/events", get(current_print_events))
        .route("/api/video.mjpeg", get(video_mjpeg))
        .route("/static/{file}", get(static_asset))
        .with_state(state);

    let bind = bind.to_string();
    let address: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid bind address {bind}"))?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind {address}"))?;
    info!(%address, "serving Bambu overlay");
    axum::serve(listener, app)
        .await
        .context("HTTP server failed")
}

fn app_state(
    mqtt: MqttRuntime,
    devices: &ResolvedDevices,
    video_endpoints: ResolvedVideoEndpoints,
) -> Result<AppState> {
    let known_devices = KnownDevices {
        devices: devices.catalog.clone(),
        local_ids: devices.local_ids.clone(),
        probed_video_ids: video_endpoints.probed_device_ids.clone(),
    };
    let snapshot = SnapshotService::new(devices.catalog.clone(), mqtt.clone());
    let video = VideoRuntime::new(
        devices.catalog.clone(),
        video_endpoints.endpoints,
        video_endpoints.endpoint_map,
    )?;

    Ok(AppState {
        snapshot,
        mqtt,
        video,
        devices: known_devices,
    })
}

impl KnownDevices {
    fn payload(&self, runtime_video_ids: &HashSet<String>) -> KnownDevicesPayload {
        KnownDevicesPayload {
            devices: self
                .devices
                .iter()
                .map(|device| self.device(device, runtime_video_ids))
                .collect(),
        }
    }

    fn device(
        &self,
        device: &CloudDevice,
        runtime_video_ids: &HashSet<String>,
    ) -> KnownDevicePayload {
        let id = device.id.clone();
        let source = match id.as_deref() {
            Some(id) if self.local_ids.contains(id) => DeviceSource::Local,
            _ => DeviceSource::Cloud,
        };
        let has_access_code = device.access_code.as_deref().is_some_and(has_text);
        let has_video = id
            .as_deref()
            .is_some_and(|id| self.probed_video_ids.contains(id) || runtime_video_ids.contains(id));
        let has_video = has_access_code && has_video;

        KnownDevicePayload {
            id: id.clone(),
            name: device.name.clone(),
            online: device.online,
            source,
            paths: device_paths(id.as_deref(), has_video),
        }
    }
}

fn device_paths(device_id: Option<&str>, has_video: bool) -> KnownDevicePaths {
    let Some(device_id) = device_id else {
        return KnownDevicePaths {
            horizontal: None,
            vertical: None,
            video: None,
        };
    };

    let query = format!("device={}", encode_query_value(device_id));
    KnownDevicePaths {
        horizontal: Some(format!("/overlay?{query}")),
        vertical: Some(format!("/vertical?{query}")),
        video: has_video.then(|| format!("/api/video.mjpeg?{query}")),
    }
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(hex(byte >> 4));
                encoded.push(hex(byte & 0x0f));
            }
        }
    }
    encoded
}

fn hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + nibble - 10) as char,
        _ => unreachable!("nibble must be four bits"),
    }
}

fn has_text(value: &str) -> bool {
    !value.trim().is_empty()
}

async fn horizontal_overlay() -> Result<Html<String>, Response> {
    render_overlay("horizontal").map(Html).map_err(render_error)
}

async fn vertical_overlay() -> Result<Html<String>, Response> {
    render_overlay("vertical").map(Html).map_err(render_error)
}

async fn current_print(State(state): State<AppState>) -> Response {
    match state.snapshot.payload().await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => {
            let payload = error_payload(error.to_string(), state.mqtt.status().await);
            (StatusCode::BAD_GATEWAY, Json(payload)).into_response()
        }
    }
}

async fn known_devices(State(state): State<AppState>) -> Json<KnownDevicesPayload> {
    let runtime_video_ids = state.video.known_device_ids().await;
    Json(state.devices.payload(&runtime_video_ids))
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
    let payload = match state.snapshot.payload().await {
        Ok(payload) => serde_json::to_string(&payload),
        Err(error) => {
            let payload = error_payload(error.to_string(), state.mqtt.status().await);
            serde_json::to_string(&payload)
        }
    }
    .unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()}).to_string());

    Event::default().event("current-print").data(payload)
}

async fn video_mjpeg(State(state): State<AppState>, Query(query): Query<DeviceQuery>) -> Response {
    let subscription = match state.video.subscribe(query.device.as_deref()).await {
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::KnownDevices;
    use crate::bambu::CloudDevice;

    #[test]
    fn known_devices_payload_includes_paths_without_access_codes() {
        let devices = KnownDevices {
            devices: vec![
                CloudDevice {
                    id: Some("printer a/1".to_owned()),
                    name: Some("Office".to_owned()),
                    online: Some(true),
                    access_code: Some("12345678".to_owned()),
                    ..CloudDevice::default()
                },
                CloudDevice {
                    id: Some("printer-b".to_owned()),
                    name: Some("Garage".to_owned()),
                    online: Some(false),
                    access_code: Some("87654321".to_owned()),
                    ..CloudDevice::default()
                },
            ],
            local_ids: HashSet::from(["printer a/1".to_owned()]),
            probed_video_ids: HashSet::from(["printer a/1".to_owned()]),
        };

        let value = serde_json::to_value(devices.payload(&HashSet::new())).unwrap();
        let json = value.to_string();
        assert!(!json.contains("12345678"));
        assert!(!json.contains("87654321"));
        assert!(!json.contains("accessCode"));
        assert_eq!(value["devices"][0]["source"], "local");
        assert_eq!(
            value["devices"][0]["paths"]["horizontal"],
            "/overlay?device=printer%20a%2F1"
        );
        assert_eq!(
            value["devices"][0]["paths"]["vertical"],
            "/vertical?device=printer%20a%2F1"
        );
        assert_eq!(
            value["devices"][0]["paths"]["video"],
            "/api/video.mjpeg?device=printer%20a%2F1"
        );
        assert_eq!(value["devices"][1]["source"], "cloud");
        assert!(value["devices"][1]["paths"].get("video").is_none());

        let value = serde_json::to_value(devices.payload(&HashSet::from(["printer-b".to_owned()])))
            .unwrap();
        assert_eq!(
            value["devices"][1]["paths"]["video"],
            "/api/video.mjpeg?device=printer-b"
        );
    }
}
