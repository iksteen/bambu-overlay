use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{bambu::PrinterStatus, device_tls, local::LocalDevice};

const KEEPALIVE: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct MqttRuntime {
    inner: Arc<RwLock<MqttState>>,
    changes: broadcast::Sender<()>,
}

#[derive(Default)]
struct MqttState {
    reports: HashMap<String, PrinterStatus>,
    connections: HashMap<String, MqttConnectionState>,
    connected: bool,
    error: Option<String>,
    updated_at: Option<String>,
}

#[derive(Default)]
struct MqttConnectionState {
    connected: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MqttStatusPayload {
    pub connected: bool,
    pub error: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Serialize)]
struct PushAllRequest {
    pushing: PushAllCommand,
}

#[derive(Serialize)]
struct PushAllCommand {
    sequence_id: String,
    command: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    push_target: Option<u8>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ReportPayload {
    Wrapped { print: PrinterStatus },
    Bare(PrinterStatus),
}

impl ReportPayload {
    fn into_report(self) -> PrinterStatus {
        match self {
            ReportPayload::Wrapped { print } => print,
            ReportPayload::Bare(report) => report,
        }
    }
}

impl MqttRuntime {
    pub fn new() -> Self {
        let (changes, _) = broadcast::channel(128);
        Self {
            inner: Arc::new(RwLock::new(MqttState::default())),
            changes,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changes.subscribe()
    }

    pub async fn reports(&self) -> HashMap<String, PrinterStatus> {
        self.inner.read().await.reports.clone()
    }

    pub async fn status(&self) -> MqttStatusPayload {
        let state = self.inner.read().await;
        MqttStatusPayload {
            connected: state.connected,
            error: state.error.clone(),
            updated_at: state.updated_at.clone(),
        }
    }

    async fn set_connected(&self, connected: bool) {
        self.set_connection_connected("cloud", connected).await;
    }

    pub async fn set_connection_connected(&self, key: impl Into<String>, connected: bool) {
        let mut state = self.inner.write().await;
        let connection = state.connections.entry(key.into()).or_default();
        connection.connected = connected;
        if connected {
            connection.error = None;
        }
        refresh_status(&mut state);
        drop(state);
        self.notify();
    }

    pub async fn set_error(&self, error: impl Into<String>) {
        self.set_connection_error("cloud", error).await;
    }

    pub async fn set_connection_error(&self, key: impl Into<String>, error: impl Into<String>) {
        let mut state = self.inner.write().await;
        let connection = state.connections.entry(key.into()).or_default();
        connection.connected = false;
        connection.error = Some(error.into());
        refresh_status(&mut state);
        drop(state);
        self.notify();
    }

    async fn merge_report(&self, device_id: &str, report: PrinterStatus) {
        let mut state = self.inner.write().await;
        let previous = state.reports.entry(device_id.to_owned()).or_default();
        previous.merge(report);
        state.updated_at = Some(chrono::Utc::now().to_rfc3339());
        refresh_status(&mut state);
        drop(state);
        self.notify();
    }

    fn notify(&self) {
        let _ = self.changes.send(());
    }
}

fn refresh_status(state: &mut MqttState) {
    state.connected = state
        .connections
        .values()
        .any(|connection| connection.connected);
    let mut errors = state
        .connections
        .iter()
        .filter_map(|(key, connection)| {
            connection
                .error
                .as_ref()
                .map(|error| format!("{key}: {error}"))
        })
        .collect::<Vec<_>>();
    errors.sort();
    state.error = (!errors.is_empty()).then(|| errors.join("; "));
}

impl Default for MqttRuntime {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn supervise(
    runtime: MqttRuntime,
    host: String,
    port: u16,
    user_id: String,
    access_token: String,
    device_ids: Vec<String>,
) {
    let mut delay = Duration::from_secs(2);
    loop {
        match run_once(&runtime, &host, port, &user_id, &access_token, &device_ids).await {
            Ok(()) => delay = Duration::from_secs(2),
            Err(error) => {
                runtime.set_error(error.to_string()).await;
                warn!(%error, "MQTT disconnected");
                tokio::time::sleep(delay).await;
                delay = (delay + delay / 2).min(Duration::from_secs(30));
            }
        }
    }
}

pub async fn supervise_local(runtime: MqttRuntime, device: LocalDevice) {
    let mut delay = Duration::from_secs(2);
    loop {
        match run_local_once(&runtime, &device).await {
            Ok(()) => delay = Duration::from_secs(2),
            Err(error) => {
                runtime
                    .set_connection_error(device.id.clone(), error.to_string())
                    .await;
                warn!(
                    device_id = %device.id,
                    host = %device.host,
                    error = %error,
                    "local MQTT disconnected"
                );
                tokio::time::sleep(delay).await;
                delay = (delay + delay / 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn run_once(
    runtime: &MqttRuntime,
    host: &str,
    port: u16,
    user_id: &str,
    access_token: &str,
    device_ids: &[String],
) -> Result<()> {
    let username = if user_id.starts_with("u_") {
        user_id.to_owned()
    } else {
        format!("u_{user_id}")
    };
    let mut options = MqttOptions::new(format!("bambu-overlay-{}", Uuid::new_v4()), host, port);
    options.set_keep_alive(KEEPALIVE);
    options.set_credentials(username, access_token);
    options.set_transport(default_mqtt_transport()?);

    let (client, mut eventloop) = AsyncClient::new(options, 32);
    for device_id in device_ids {
        client
            .subscribe(format!("device/{device_id}/report"), QoS::AtMostOnce)
            .await
            .with_context(|| format!("failed to subscribe to {device_id}"))?;
    }
    for (sequence_id, device_id) in device_ids.iter().enumerate() {
        client
            .publish(
                format!("device/{device_id}/request"),
                QoS::AtMostOnce,
                false,
                serde_json::to_vec(&PushAllRequest {
                    pushing: PushAllCommand {
                        sequence_id: sequence_id.to_string(),
                        command: "pushall",
                        version: None,
                        push_target: None,
                    },
                })?,
            )
            .await
            .with_context(|| format!("failed to request pushall for {device_id}"))?;
    }

    runtime.set_connected(true).await;
    loop {
        match eventloop.poll().await? {
            Event::Incoming(Packet::Publish(publish)) => {
                handle_publish(runtime, publish.topic, publish.payload.to_vec()).await;
            }
            Event::Incoming(Packet::Disconnect) => break,
            _ => {}
        }
    }
    runtime.set_connected(false).await;
    Ok(())
}

async fn run_local_once(runtime: &MqttRuntime, device: &LocalDevice) -> Result<()> {
    let access_code = device
        .access_code
        .as_deref()
        .with_context(|| format!("local device {} does not include an access code", device.id))?;
    let mut options = MqttOptions::new(
        format!("bambu-overlay-{}", Uuid::new_v4()),
        device.host.as_str(),
        device.mqtt_port,
    );
    options.set_keep_alive(KEEPALIVE);
    options.set_credentials("bblp", access_code);
    options.set_transport(local_mqtt_transport()?);

    let (client, mut eventloop) = AsyncClient::new(options, 32);
    client
        .subscribe(format!("device/{}/report", device.id), QoS::AtMostOnce)
        .await
        .with_context(|| format!("failed to subscribe to local device {}", device.id))?;
    client
        .publish(
            format!("device/{}/request", device.id),
            QoS::AtMostOnce,
            false,
            serde_json::to_vec(&PushAllRequest {
                pushing: PushAllCommand {
                    sequence_id: "0".to_owned(),
                    command: "pushall",
                    version: Some(1),
                    push_target: Some(1),
                },
            })?,
        )
        .await
        .with_context(|| format!("failed to request pushall for local device {}", device.id))?;

    runtime
        .set_connection_connected(device.id.clone(), true)
        .await;
    loop {
        match eventloop.poll().await? {
            Event::Incoming(Packet::Publish(publish)) => {
                handle_publish(runtime, publish.topic, publish.payload.to_vec()).await;
            }
            Event::Incoming(Packet::Disconnect) => break,
            _ => {}
        }
    }
    runtime
        .set_connection_connected(device.id.clone(), false)
        .await;
    Ok(())
}

fn default_mqtt_transport() -> Result<Transport> {
    let connector =
        native_tls::TlsConnector::new().context("failed to build default MQTT TLS connector")?;
    Ok(Transport::tls_with_config(connector.into()))
}

fn local_mqtt_transport() -> Result<Transport> {
    Ok(Transport::tls_with_config(
        device_tls::native_connector()?.into(),
    ))
}

async fn handle_publish(runtime: &MqttRuntime, topic: String, payload: Vec<u8>) {
    let parts = topic.split('/').collect::<Vec<_>>();
    if parts.len() != 3 || parts[0] != "device" || parts[2] != "report" {
        debug!(%topic, "ignoring unexpected MQTT topic");
        return;
    }
    let Ok(report) = serde_json::from_slice::<ReportPayload>(&payload) else {
        warn!(
            topic = %topic,
            payload = %payload_preview(&payload),
            "ignoring MQTT report with unexpected JSON shape"
        );
        return;
    };
    runtime.merge_report(parts[1], report.into_report()).await;
}

fn payload_preview(payload: &[u8]) -> String {
    let limit = payload.len().min(300);
    let mut preview = String::from_utf8_lossy(&payload[..limit]).into_owned();
    if payload.len() > limit {
        preview.push_str("...");
    }
    preview
}
