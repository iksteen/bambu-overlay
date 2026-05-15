use std::time::Duration;

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::{bambu::PrinterStatus, device_tls, local::LocalDevice};

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

pub(crate) fn start_local_supervisors(runtime: MqttRuntime, devices: Vec<LocalDevice>) {
    for device in devices {
        start_local_supervisor(runtime.clone(), device);
    }
}

fn start_local_supervisor(runtime: MqttRuntime, device: LocalDevice) {
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

pub(crate) async fn supervise_cloud(
    runtime: MqttRuntime,
    host: String,
    port: u16,
    user_id: String,
    access_token: String,
    device_ids: Vec<String>,
) {
    let mut delay = Duration::from_secs(2);
    loop {
        match run_cloud_once(&runtime, &host, port, &user_id, &access_token, &device_ids).await {
            Ok(()) => delay = Duration::from_secs(2),
            Err(error) => {
                runtime.set_cloud_error(error.to_string()).await;
                warn!(%error, "cloud MQTT disconnected");
                tokio::time::sleep(delay).await;
                delay = (delay + delay / 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn supervise_local(runtime: MqttRuntime, device: LocalDevice) {
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
                    host = %device.endpoint.host,
                    error = %error,
                    "local MQTT disconnected"
                );
                tokio::time::sleep(delay).await;
                delay = (delay + delay / 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn run_cloud_once(
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

    runtime.set_cloud_connected(true).await;
    loop {
        match eventloop.poll().await? {
            Event::Incoming(Packet::Publish(publish)) => {
                handle_publish(runtime, publish.topic, publish.payload.to_vec()).await;
            }
            Event::Incoming(Packet::Disconnect) => break,
            _ => {}
        }
    }
    runtime.set_cloud_connected(false).await;
    Ok(())
}

async fn run_local_once(runtime: &MqttRuntime, device: &LocalDevice) -> Result<()> {
    let mut options = MqttOptions::new(
        format!("bambu-overlay-{}", Uuid::new_v4()),
        device.endpoint.host.as_str(),
        device.endpoint.port,
    );
    options.set_keep_alive(KEEPALIVE);
    options.set_credentials("bblp", device.endpoint.access_code.as_str());
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
