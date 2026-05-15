use anyhow::Result;
use chrono::Utc;
use serde::Serialize;

use crate::{
    devices::KnownDevice,
    mqtt::{MqttRuntime, MqttStatusPayload},
};

use super::summary::{overlay_device, summarize_devices, OverlayDevice};

#[derive(Clone)]
pub struct SnapshotService {
    devices: Vec<KnownDevice>,
    mqtt: MqttRuntime,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayPayload {
    ok: bool,
    updated_at: String,
    mqtt: MqttStatusPayload,
    devices: Vec<OverlayDevice>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorPayload {
    ok: bool,
    error: String,
    updated_at: String,
    mqtt: MqttStatusPayload,
    devices: Vec<OverlayDevice>,
}

impl SnapshotService {
    pub(crate) fn new(devices: Vec<KnownDevice>, mqtt: MqttRuntime) -> Self {
        Self { devices, mqtt }
    }

    pub async fn payload(&self) -> Result<OverlayPayload> {
        let reports = self.mqtt.reports().await;
        let status = self.mqtt.status().await;
        let devices = summarize_devices(&self.devices, &reports)
            .into_iter()
            .map(overlay_device)
            .collect();

        Ok(OverlayPayload {
            ok: true,
            updated_at: Utc::now().to_rfc3339(),
            mqtt: status,
            devices,
        })
    }
}

pub fn error_payload(error: impl Into<String>, mqtt: MqttStatusPayload) -> ErrorPayload {
    ErrorPayload {
        ok: false,
        error: error.into(),
        updated_at: Utc::now().to_rfc3339(),
        mqtt,
        devices: Vec::new(),
    }
}
