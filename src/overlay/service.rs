use anyhow::Result;
use chrono::Utc;
use serde::Serialize;

use crate::{
    bambu::{CloudDevice, CurrentPrintResponse, TasksResponse},
    mqtt::{MqttRuntime, MqttStatusPayload},
};

use super::summary::{overlay_device, summarize_devices, OverlayDevice};

#[derive(Clone)]
pub struct SnapshotService {
    devices: Vec<CloudDevice>,
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
    pub fn new(devices: Vec<CloudDevice>, mqtt: MqttRuntime) -> Self {
        Self { devices, mqtt }
    }

    pub async fn payload(&self, _force_cloud_refresh: bool) -> Result<OverlayPayload> {
        let current_print = CurrentPrintResponse {
            devices: self.devices.clone(),
        };
        let tasks = TasksResponse::default();
        let reports = self.mqtt.reports().await;
        let status = self.mqtt.status().await;
        let devices = summarize_devices(&current_print, &tasks, &reports)
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
