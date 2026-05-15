use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncWriteExt};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::{
    bambu::PrinterStatus,
    device_tls,
    local::{LocalDevice, MqttEndpoint},
};

use super::MqttRuntime;

const KEEPALIVE: Duration = Duration::from_secs(60);

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

#[derive(Clone)]
pub(crate) enum MqttTarget {
    Cloud {
        endpoint: MqttEndpoint,
        user_id: String,
        access_token: String,
        device_ids: Vec<String>,
    },
    Local(LocalDevice),
}

struct ReportSession {
    eventloop: EventLoop,
    topics: HashSet<String>,
}

struct ReportEvent {
    topic: String,
    payload: Vec<u8>,
}

impl MqttTarget {
    pub(crate) fn cloud(
        endpoint: MqttEndpoint,
        user_id: String,
        access_token: String,
        device_ids: Vec<String>,
    ) -> Self {
        Self::Cloud {
            endpoint,
            user_id,
            access_token,
            device_ids,
        }
    }

    pub(crate) fn local(device: LocalDevice) -> Self {
        Self::Local(device)
    }

    fn device_ids(&self) -> Vec<String> {
        match self {
            MqttTarget::Cloud { device_ids, .. } => device_ids.clone(),
            MqttTarget::Local(device) => vec![device.id.clone()],
        }
    }

    fn connection_key(&self) -> String {
        match self {
            MqttTarget::Cloud { .. } => "cloud".to_owned(),
            MqttTarget::Local(device) => device.id.clone(),
        }
    }

    fn options(&self) -> Result<MqttOptions> {
        match self {
            MqttTarget::Cloud {
                endpoint,
                user_id,
                access_token,
                ..
            } => cloud_mqtt_options(endpoint, user_id, access_token),
            MqttTarget::Local(device) => local_mqtt_options(device),
        }
    }

    fn pushall(&self, sequence_id: String) -> PushAllRequest {
        match self {
            MqttTarget::Cloud { .. } => cloud_pushall(sequence_id),
            MqttTarget::Local(_) => local_pushall(),
        }
    }

    fn warn_disconnect(&self, error: &anyhow::Error, label: &'static str) {
        match self {
            MqttTarget::Cloud { .. } => warn!(%error, "{label}"),
            MqttTarget::Local(device) => {
                warn!(
                    device_id = %device.id,
                    host = %device.endpoint.host(),
                    error = %error,
                    "{label}"
                );
            }
        }
    }
}

impl ReportSession {
    async fn connect(target: &MqttTarget) -> Result<Self> {
        let device_ids = target.device_ids();
        let topics = device_ids
            .iter()
            .map(|device_id| format!("device/{device_id}/report"))
            .collect::<HashSet<_>>();
        let options = target.options()?;
        let (client, eventloop) = AsyncClient::new(options, 32);

        for device_id in &device_ids {
            subscribe_report(&client, device_id)
                .await
                .with_context(|| format!("failed to subscribe to {device_id}"))?;
        }
        for (sequence_id, device_id) in device_ids.iter().enumerate() {
            request_pushall(&client, device_id, target.pushall(sequence_id.to_string()))
                .await
                .with_context(|| format!("failed to request pushall for {device_id}"))?;
        }

        Ok(Self { eventloop, topics })
    }

    async fn next(&mut self) -> Result<Option<ReportEvent>> {
        loop {
            match self.eventloop.poll().await? {
                Event::Incoming(Packet::Publish(publish))
                    if self.topics.contains(&publish.topic) =>
                {
                    return Ok(Some(ReportEvent {
                        topic: publish.topic,
                        payload: publish.payload.to_vec(),
                    }));
                }
                Event::Incoming(Packet::Publish(publish)) => {
                    debug!(topic = %publish.topic, "ignoring unexpected MQTT topic");
                }
                Event::Incoming(Packet::Disconnect) => return Ok(None),
                _ => {}
            }
        }
    }
}

pub(crate) fn start_local_supervisors(runtime: MqttRuntime, devices: Vec<LocalDevice>) {
    for device in devices {
        start_local_supervisor(runtime.clone(), MqttTarget::local(device));
    }
}

fn start_local_supervisor(runtime: MqttRuntime, target: MqttTarget) {
    let device_id = target.connection_key();
    let mqtt_status = runtime.clone();
    let supervisor = tokio::spawn(supervise_target(runtime, target));
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

pub(crate) async fn supervise_target(runtime: MqttRuntime, target: MqttTarget) {
    let mut delay = Duration::from_secs(2);
    loop {
        match run_runtime_once(&runtime, &target).await {
            Ok(()) => delay = Duration::from_secs(2),
            Err(error) => {
                runtime
                    .set_connection_error(target.connection_key(), error.to_string())
                    .await;
                target.warn_disconnect(&error, "MQTT disconnected");
                tokio::time::sleep(delay).await;
                delay = (delay + delay / 2).min(Duration::from_secs(30));
            }
        }
    }
}

pub(crate) async fn monitor_target(target: MqttTarget) -> Result<()> {
    let mut delay = Duration::from_secs(2);
    loop {
        tokio::select! {
            result = run_monitor_once(&target) => {
                match result {
                    Ok(()) => delay = Duration::from_secs(2),
                    Err(error) => {
                        target.warn_disconnect(&error, "MQTT monitor disconnected");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        delay = (delay + delay / 2).min(Duration::from_secs(30));
    }
}

async fn run_runtime_once(runtime: &MqttRuntime, target: &MqttTarget) -> Result<()> {
    let mut session = ReportSession::connect(target).await?;
    let connection_key = target.connection_key();
    runtime
        .set_connection_connected(connection_key.clone(), true)
        .await;
    while let Some(event) = session.next().await? {
        handle_publish(runtime, event.topic, event.payload).await;
    }
    runtime
        .set_connection_connected(connection_key, false)
        .await;
    Ok(())
}

async fn run_monitor_once(target: &MqttTarget) -> Result<()> {
    let mut session = ReportSession::connect(target).await?;
    let mut stdout = io::stdout();
    while let Some(event) = session.next().await? {
        stdout.write_all(&event.payload).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }
    Ok(())
}

fn cloud_mqtt_options(
    endpoint: &MqttEndpoint,
    user_id: &str,
    access_token: &str,
) -> Result<MqttOptions> {
    let username = if user_id.starts_with("u_") {
        user_id.to_owned()
    } else {
        format!("u_{user_id}")
    };
    let mut options = MqttOptions::new(
        format!("bambu-overlay-{}", Uuid::new_v4()),
        endpoint.host.as_str(),
        endpoint.port,
    );
    options.set_keep_alive(KEEPALIVE);
    options.set_credentials(username, access_token);
    options.set_transport(default_mqtt_transport()?);
    Ok(options)
}

fn local_mqtt_options(device: &LocalDevice) -> Result<MqttOptions> {
    let mut options = MqttOptions::new(
        format!("bambu-overlay-{}", Uuid::new_v4()),
        device.endpoint.host(),
        device.endpoint.port(),
    );
    options.set_keep_alive(KEEPALIVE);
    options.set_credentials("bblp", device.endpoint.access_code.as_str());
    options.set_transport(local_mqtt_transport()?);
    Ok(options)
}

async fn subscribe_report(client: &AsyncClient, device_id: &str) -> Result<()> {
    client
        .subscribe(format!("device/{device_id}/report"), QoS::AtMostOnce)
        .await?;
    Ok(())
}

async fn request_pushall(
    client: &AsyncClient,
    device_id: &str,
    request: PushAllRequest,
) -> Result<()> {
    client
        .publish(
            format!("device/{device_id}/request"),
            QoS::AtMostOnce,
            false,
            serde_json::to_vec(&request)?,
        )
        .await?;
    Ok(())
}

fn cloud_pushall(sequence_id: String) -> PushAllRequest {
    PushAllRequest {
        pushing: PushAllCommand {
            sequence_id,
            command: "pushall",
            version: None,
            push_target: None,
        },
    }
}

fn local_pushall() -> PushAllRequest {
    PushAllRequest {
        pushing: PushAllCommand {
            sequence_id: "0".to_owned(),
            command: "pushall",
            version: Some(1),
            push_target: Some(1),
        },
    }
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
