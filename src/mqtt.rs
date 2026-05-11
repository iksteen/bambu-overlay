use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::bambu::{BambuClient, PrinterStatus};

const KEEPALIVE: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct MqttRuntime {
    inner: Arc<RwLock<MqttState>>,
    changes: broadcast::Sender<()>,
}

#[derive(Default)]
struct MqttState {
    reports: HashMap<String, PrinterStatus>,
    connected: bool,
    error: Option<String>,
    updated_at: Option<String>,
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

    pub async fn set_disabled(&self, reason: impl Into<String>) {
        let mut state = self.inner.write().await;
        state.connected = false;
        state.error = Some(format!("MQTT disabled: {}", reason.into()));
        drop(state);
        self.notify();
    }

    async fn set_connected(&self, connected: bool) {
        let mut state = self.inner.write().await;
        state.connected = connected;
        if connected {
            state.error = None;
        }
        drop(state);
        self.notify();
    }

    pub async fn set_error(&self, error: impl Into<String>) {
        let mut state = self.inner.write().await;
        state.connected = false;
        state.error = Some(error.into());
        drop(state);
        self.notify();
    }

    async fn merge_report(&self, device_id: &str, report: PrinterStatus) {
        let mut state = self.inner.write().await;
        let previous = state.reports.entry(device_id.to_owned()).or_default();
        previous.merge(report);
        state.connected = true;
        state.error = None;
        state.updated_at = Some(chrono::Utc::now().to_rfc3339());
        drop(state);
        self.notify();
    }

    fn notify(&self) {
        let _ = self.changes.send(());
    }
}

impl Default for MqttRuntime {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn initial_device_ids(client: &BambuClient, access_token: &str) -> Result<Vec<String>> {
    let current_print = client.current_print(access_token).await?;
    Ok(current_print
        .devices
        .into_iter()
        .filter_map(|device| device.id)
        .collect())
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
    options.set_transport(Transport::tls_with_default_config());

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
