use std::{collections::HashMap, sync::Arc};

use serde::Serialize;
use tokio::sync::{broadcast, RwLock};

use crate::bambu::PrinterStatus;

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

    pub(crate) async fn set_cloud_connected(&self, connected: bool) {
        self.set_connection_connected("cloud", connected).await;
    }

    pub(crate) async fn set_cloud_error(&self, error: impl Into<String>) {
        self.set_connection_error("cloud", error).await;
    }

    pub(crate) async fn set_connection_connected(&self, key: impl Into<String>, connected: bool) {
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

    pub(crate) async fn set_connection_error(
        &self,
        key: impl Into<String>,
        error: impl Into<String>,
    ) {
        let mut state = self.inner.write().await;
        let connection = state.connections.entry(key.into()).or_default();
        connection.connected = false;
        connection.error = Some(error.into());
        refresh_status(&mut state);
        drop(state);
        self.notify();
    }

    pub(crate) async fn merge_report(&self, device_id: &str, report: PrinterStatus) {
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
