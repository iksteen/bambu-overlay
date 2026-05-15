use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    net::SocketAddr,
    time::Duration,
};

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
use tracing::{debug, error, info, warn};

use crate::{
    assets,
    bambu::{CloudDevice, MQTT_HOST, MQTT_PORT},
    cloud::{cloud_devices, cloud_mqtt_device_ids, cloud_mqtt_startup, start_cloud_mqtt},
    local::{
        infer_local_device_id, CloudDeviceConfig, Endpoint, LocalDevice, LocalDeviceConfig,
        MqttEndpoint,
    },
    mqtt::{supervise_local, MqttRuntime},
    overlay::{error_payload, SnapshotService},
    video::{
        infer_video_device_id, mjpeg_content_type, probe_video_endpoint, VideoEndpoint,
        VideoRuntime, DEFAULT_VIDEO_PORT,
    },
};

pub use crate::cloud::CloudSession;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8765;

#[derive(Clone)]
pub struct ServerConfig {
    pub bind: Endpoint,
    pub cloud_mqtt: MqttEndpoint,
    pub no_cloud_enum: bool,
    pub local_devices: Vec<LocalDeviceConfig>,
    pub cloud_devices: Vec<CloudDeviceConfig>,
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

struct StartupDevices {
    catalog: Vec<CloudDevice>,
    local: Vec<LocalDevice>,
    local_ids: HashSet<String>,
    cloud_mqtt_ids: Vec<String>,
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
    let startup = startup_devices(&cloud, &config).await?;
    let cloud_mqtt =
        cloud_mqtt_startup(cloud.as_ref(), &config.cloud_mqtt, &startup.cloud_mqtt_ids).await?;
    let video = startup_video(&config.video_endpoints, &startup).await?;
    let state = app_state(mqtt.clone(), &startup, video)?;

    start_cloud_mqtt(mqtt.clone(), cloud_mqtt);
    start_local_mqtt(mqtt, startup.local);
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

async fn startup_devices(
    cloud: &Option<CloudSession>,
    config: &ServerConfig,
) -> Result<StartupDevices> {
    let cloud_devices =
        cloud_devices(cloud.as_ref(), &config.cloud_devices, config.no_cloud_enum).await?;
    let mut local = resolve_local_devices(config.local_devices.clone()).await?;
    ensure_unique_local_device_ids(&local)?;
    merge_local_metadata(&mut local, &cloud_devices.metadata);
    ensure_local_names(&mut local);
    ensure_local_access_codes(&local)?;

    let local_ids = local_device_ids(&local);
    let catalog = catalog_devices(cloud_devices.catalog, &local, &local_ids);
    if catalog.is_empty() {
        anyhow::bail!(
            "no devices configured; run `bambu-overlay login`, set --cloud-device, or set --local-device"
        );
    }

    let cloud_mqtt_ids = cloud_mqtt_device_ids(&catalog, &local_ids);
    Ok(StartupDevices {
        catalog,
        local,
        local_ids,
        cloud_mqtt_ids,
    })
}

async fn startup_video(
    configured: &[VideoEndpoint],
    devices: &StartupDevices,
) -> Result<VideoEndpointCatalog> {
    let catalog_ids = catalog_device_ids(&devices.catalog);
    video_endpoints(configured, &devices.local, &catalog_ids).await
}

fn app_state(
    mqtt: MqttRuntime,
    devices: &StartupDevices,
    video_endpoints: VideoEndpointCatalog,
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

fn start_local_mqtt(runtime: MqttRuntime, devices: Vec<LocalDevice>) {
    for device in devices {
        start_local_mqtt_device(runtime.clone(), device);
    }
}

fn start_local_mqtt_device(runtime: MqttRuntime, device: LocalDevice) {
    let device_id = device.id.clone();
    let mqtt_status = runtime.clone();
    let supervisor = tokio::spawn(supervise_local(runtime, device));
    tokio::spawn(async move {
        match supervisor.await {
            Ok(()) => {
                warn!(
                    device_id = %device_id,
                    "local MQTT supervisor exited unexpectedly"
                );
                mqtt_status
                    .set_connection_error(device_id, "local MQTT supervisor exited unexpectedly")
                    .await;
            }
            Err(error) => {
                error!(
                    device_id = %device_id,
                    error = %error,
                    "local MQTT supervisor task failed"
                );
                mqtt_status
                    .set_connection_error(
                        device_id,
                        format!("local MQTT supervisor task failed: {error}"),
                    )
                    .await;
            }
        }
    });
}

async fn resolve_local_devices(configs: Vec<LocalDeviceConfig>) -> Result<Vec<LocalDevice>> {
    let mut devices = Vec::with_capacity(configs.len());
    for config in configs {
        let id = infer_local_device_id(&config).await.with_context(|| {
            format!(
                "could not infer device ID for --local-device `{}`",
                config.mqtt_endpoint()
            )
        })?;
        info!(
            device_id = %id,
            endpoint = %config.mqtt_endpoint(),
            "inferred local device ID from MQTT certificate"
        );
        devices.push(config.into_device(id));
    }
    Ok(devices)
}

fn ensure_unique_local_device_ids(local_devices: &[LocalDevice]) -> Result<()> {
    let mut seen = HashSet::new();
    for device in local_devices {
        if !seen.insert(device.id.as_str()) {
            anyhow::bail!(
                "--local-device resolves duplicate device id `{}`",
                device.id
            );
        }
    }
    Ok(())
}

fn merge_local_metadata(local_devices: &mut [LocalDevice], cloud_devices: &[CloudDevice]) {
    for local in local_devices {
        if let Some(cloud) = cloud_devices
            .iter()
            .find(|cloud| cloud.id.as_deref() == Some(local.id.as_str()))
        {
            local.merge_cloud_metadata(cloud);
        }
    }
}

fn ensure_local_names(local_devices: &mut [LocalDevice]) {
    for device in local_devices {
        if device
            .name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
        {
            device.name = Some(device.id.clone());
        }
    }
}

fn ensure_local_access_codes(local_devices: &[LocalDevice]) -> Result<()> {
    for device in local_devices {
        if device
            .access_code
            .as_deref()
            .is_none_or(|code| code.trim().is_empty())
        {
            anyhow::bail!(
                "--local-device `{}` is missing an access code; provide ACCESS_CODE or cloud metadata that exposes dev_access_code",
                device.id
            );
        }
    }
    Ok(())
}

fn local_device_ids(local_devices: &[LocalDevice]) -> HashSet<String> {
    local_devices
        .iter()
        .map(|device| device.id.clone())
        .collect()
}

fn catalog_devices(
    cloud_devices: Vec<CloudDevice>,
    local_devices: &[LocalDevice],
    local_ids: &HashSet<String>,
) -> Vec<CloudDevice> {
    let mut devices = cloud_devices
        .into_iter()
        .filter(|device| {
            device
                .id
                .as_deref()
                .is_none_or(|device_id| !local_ids.contains(device_id))
        })
        .collect::<Vec<_>>();
    devices.extend(local_devices.iter().map(LocalDevice::cloud_device));
    devices
}

fn catalog_device_ids(devices: &[CloudDevice]) -> HashSet<String> {
    devices
        .iter()
        .filter_map(|device| device.id.clone())
        .collect()
}

struct VideoEndpointCatalog {
    endpoints: Vec<VideoEndpoint>,
    probed_device_ids: HashSet<String>,
    endpoint_map: HashMap<String, VideoEndpoint>,
}

async fn video_endpoints(
    explicit: &[VideoEndpoint],
    local_devices: &[LocalDevice],
    catalog_device_ids: &HashSet<String>,
) -> Result<VideoEndpointCatalog> {
    let mut endpoints = Vec::with_capacity(explicit.len() + local_devices.len());
    let mut probed_device_ids = HashSet::new();
    let mut endpoint_map = HashMap::new();
    let mut candidates = Vec::new();
    let mut probes = tokio::task::JoinSet::new();

    for endpoint in explicit {
        let device_id = infer_video_device_id(endpoint).await.with_context(|| {
            format!("could not infer device ID for --video-device `{endpoint}`")
        })?;
        ensure_video_device_exists(endpoint, &device_id, catalog_device_ids)?;
        info!(
            device_id = %device_id,
            endpoint = %endpoint,
            "validated explicit local video endpoint"
        );
        endpoints.push(endpoint.clone());
        candidates.push(endpoint.clone());
        probed_device_ids.insert(device_id.clone());
        endpoint_map.insert(device_id, endpoint.clone());
    }

    for device in local_devices {
        let endpoint = local_video_endpoint(device);
        if candidates.iter().any(|candidate| candidate == &endpoint) {
            continue;
        }

        candidates.push(endpoint.clone());
        let device_id = device.id.clone();
        probes.spawn(async move {
            let result = probe_video_endpoint(&device_id, &endpoint).await;
            (device_id, endpoint, result)
        });
    }

    while let Some(result) = probes.join_next().await {
        match result {
            Ok((device_id, endpoint, Ok(()))) => {
                info!(
                    device_id = %device_id,
                    endpoint = %endpoint,
                    "auto-enabled local video endpoint"
                );
                probed_device_ids.insert(device_id.clone());
                endpoint_map.insert(device_id, endpoint.clone());
                endpoints.push(endpoint);
            }
            Ok((device_id, endpoint, Err(error))) => {
                debug!(
                    device_id = %device_id,
                    endpoint = %endpoint,
                    error = %error,
                    "local video endpoint probe failed"
                );
            }
            Err(error) => {
                debug!(%error, "local video endpoint probe task failed");
            }
        }
    }

    Ok(VideoEndpointCatalog {
        endpoints,
        probed_device_ids,
        endpoint_map,
    })
}

fn ensure_video_device_exists(
    endpoint: &VideoEndpoint,
    device_id: &str,
    catalog_device_ids: &HashSet<String>,
) -> Result<()> {
    if !catalog_device_ids.contains(device_id) {
        anyhow::bail!(
            "--video-device `{endpoint}` is for device `{device_id}`, but no matching cloud or local device is configured"
        );
    }
    Ok(())
}

fn local_video_endpoint(device: &LocalDevice) -> VideoEndpoint {
    VideoEndpoint::new(device.host.clone(), DEFAULT_VIDEO_PORT)
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
    match state.snapshot.payload(true).await {
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

    use super::{
        catalog_device_ids, catalog_devices, ensure_local_access_codes, ensure_local_names,
        ensure_video_device_exists, local_device_ids, local_video_endpoint, merge_local_metadata,
        KnownDevices,
    };
    use crate::{
        bambu::CloudDevice,
        local::{LocalDevice, LocalDeviceConfig},
        video::VideoEndpoint,
    };

    fn local(id: &str, value: &str) -> LocalDevice {
        let config: LocalDeviceConfig = value.parse().expect("local device should parse");
        config.into_device(id.to_owned())
    }

    fn endpoint(value: &str) -> VideoEndpoint {
        value.parse().expect("video endpoint should parse")
    }

    #[test]
    fn catalog_uses_local_device_when_ids_overlap() {
        let local_devices = vec![local("printer-a", "192.168.1.50,12345678,Office,P1S")];
        let local_ids = local_device_ids(&local_devices);
        let catalog = catalog_devices(
            vec![
                CloudDevice {
                    id: Some("printer-a".to_owned()),
                    name: Some("Cloud Office".to_owned()),
                    access_code: Some("87654321".to_owned()),
                    ..CloudDevice::default()
                },
                CloudDevice {
                    id: Some("printer-b".to_owned()),
                    name: Some("Garage".to_owned()),
                    ..CloudDevice::default()
                },
            ],
            &local_devices,
            &local_ids,
        );

        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].id.as_deref(), Some("printer-b"));
        assert_eq!(catalog[1].id.as_deref(), Some("printer-a"));
        assert_eq!(catalog[1].access_code.as_deref(), Some("12345678"));
    }

    #[test]
    fn catalog_device_ids_ignores_devices_without_ids() {
        let ids = catalog_device_ids(&[
            CloudDevice {
                id: Some("printer-a".to_owned()),
                ..CloudDevice::default()
            },
            CloudDevice::default(),
        ]);

        assert_eq!(ids, HashSet::from(["printer-a".to_owned()]));
    }

    #[test]
    fn local_video_endpoint_uses_host_and_default_port() {
        let device = local("printer-a", "192.168.1.50,12345678,Office");

        assert_eq!(local_video_endpoint(&device), endpoint("192.168.1.50:6000"));
    }

    #[test]
    fn missing_local_access_code_is_backfilled_from_cloud_metadata() {
        let mut local_devices = vec![local("printer-a", "192.168.1.50,,Office,P1S")];
        let cloud_devices = vec![CloudDevice {
            id: Some("printer-a".to_owned()),
            access_code: Some("12345678".to_owned()),
            ..CloudDevice::default()
        }];

        merge_local_metadata(&mut local_devices, &cloud_devices);

        ensure_local_access_codes(&local_devices).unwrap();
        assert_eq!(local_devices[0].access_code.as_deref(), Some("12345678"));
    }

    #[test]
    fn local_device_name_defaults_to_device_id_when_missing() {
        let mut local_devices = vec![local("printer-a", "192.168.1.50,12345678")];

        ensure_local_names(&mut local_devices);

        assert_eq!(local_devices[0].name.as_deref(), Some("printer-a"));
    }

    #[test]
    fn local_device_name_keeps_explicit_or_cloud_name() {
        let mut local_devices = vec![
            local("printer-a", "192.168.1.50,12345678,Office"),
            local("printer-b", "192.168.1.51,87654321"),
        ];
        let cloud_devices = vec![CloudDevice {
            id: Some("printer-b".to_owned()),
            name: Some("Garage".to_owned()),
            ..CloudDevice::default()
        }];

        merge_local_metadata(&mut local_devices, &cloud_devices);
        ensure_local_names(&mut local_devices);

        assert_eq!(local_devices[0].name.as_deref(), Some("Office"));
        assert_eq!(local_devices[1].name.as_deref(), Some("Garage"));
    }

    #[test]
    fn missing_local_access_code_errors_when_not_backfilled() {
        let local_devices = vec![local("printer-a", "192.168.1.50")];

        let error = ensure_local_access_codes(&local_devices).unwrap_err();

        assert!(error.to_string().contains("printer-a"));
        assert!(error.to_string().contains("missing an access code"));
    }

    #[test]
    fn explicit_video_requires_matching_known_device() {
        let endpoint = endpoint("192.168.1.50");
        let error = ensure_video_device_exists(
            &endpoint,
            "printer-a",
            &HashSet::from(["printer-b".to_owned()]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("--video-device"));
        assert!(error.to_string().contains("printer-a"));
        assert!(error
            .to_string()
            .contains("no matching cloud or local device"));
    }

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
