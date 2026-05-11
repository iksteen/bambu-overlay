use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::{
    bambu::{BambuClient, CurrentPrintResponse, TasksResponse},
    mqtt::{MqttRuntime, MqttStatusPayload},
};

use super::summary::{overlay_device, summarize_devices, OverlayDevice};

#[derive(Clone)]
pub struct SnapshotService {
    client: BambuClient,
    access_token: String,
    task_limit: usize,
    cloud_refresh: Duration,
    mqtt: MqttRuntime,
    cache: Arc<Mutex<CloudCache>>,
    refresh: Arc<Mutex<()>>,
}

#[derive(Default)]
struct CloudCache {
    current_print: Option<CurrentPrintResponse>,
    tasks: Option<TasksResponse>,
    fetched_at: Option<Instant>,
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
    pub fn new(
        client: BambuClient,
        access_token: String,
        task_limit: usize,
        cloud_refresh: Duration,
        mqtt: MqttRuntime,
    ) -> Self {
        Self {
            client,
            access_token,
            task_limit,
            cloud_refresh,
            mqtt,
            cache: Arc::new(Mutex::new(CloudCache::default())),
            refresh: Arc::new(Mutex::new(())),
        }
    }

    pub async fn payload(&self, force_cloud_refresh: bool) -> Result<OverlayPayload> {
        let (current_print, tasks) = self.cloud_payload(force_cloud_refresh).await?;
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

    async fn cloud_payload(
        &self,
        force_refresh: bool,
    ) -> Result<(CurrentPrintResponse, TasksResponse)> {
        if let Some(cached) = self.cached_cloud_payload(force_refresh).await {
            return Ok(cached);
        }

        let _refresh = self.refresh.lock().await;
        if let Some(cached) = self.cached_cloud_payload(force_refresh).await {
            return Ok(cached);
        }

        let current = self.client.current_print(&self.access_token).await?;
        let tasks = self
            .client
            .tasks(&self.access_token, self.task_limit, None)
            .await?;
        self.store_cloud_payload(&current, &tasks).await;
        Ok((current, tasks))
    }

    async fn cached_cloud_payload(
        &self,
        force_refresh: bool,
    ) -> Option<(CurrentPrintResponse, TasksResponse)> {
        if force_refresh {
            return None;
        }

        let cache = self.cache.lock().await;
        if let (Some(current), Some(tasks), Some(fetched_at)) =
            (&cache.current_print, &cache.tasks, cache.fetched_at)
        {
            if fetched_at.elapsed() < self.cloud_refresh {
                return Some((current.clone(), tasks.clone()));
            }
        }
        None
    }

    async fn store_cloud_payload(&self, current: &CurrentPrintResponse, tasks: &TasksResponse) {
        let mut cache = self.cache.lock().await;
        cache.current_print = Some(current.clone());
        cache.tasks = Some(tasks.clone());
        cache.fetched_at = Some(Instant::now());
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
