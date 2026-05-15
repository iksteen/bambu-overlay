use std::collections::HashSet;

use anyhow::{Context, Result};
use tracing::{error, warn};

use crate::{
    bambu::{BambuClient, CloudDevice},
    config::DeviceConfig,
    local::MqttEndpoint,
    mqtt::{supervise_cloud, MqttRuntime},
};

#[derive(Clone)]
pub struct CloudSession {
    pub client: BambuClient,
    pub access_token: String,
}

pub(crate) struct CloudMqttStartup {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user_id: String,
    pub(crate) access_token: String,
    pub(crate) device_ids: Vec<String>,
}

pub(crate) async fn cloud_devices(
    cloud: Option<&CloudSession>,
    configs: &[DeviceConfig],
    enumerate_catalog: bool,
) -> Result<Vec<CloudDevice>> {
    let bound = if enumerate_catalog {
        bound_cloud_devices(cloud).await?
    } else {
        Vec::new()
    };
    if enumerate_catalog {
        Ok(bound)
    } else {
        Ok(explicit_cloud_devices(configs, &[]))
    }
}

pub(crate) async fn bound_cloud_devices(cloud: Option<&CloudSession>) -> Result<Vec<CloudDevice>> {
    let cloud = cloud.context("Bambu Cloud /bind metadata requires a Bambu Cloud token")?;
    let mut bound = cloud.client.bound_devices(&cloud.access_token).await?;
    for device in &mut bound.devices {
        device.status = Default::default();
    }
    Ok(bound.devices)
}

fn explicit_cloud_devices(configs: &[DeviceConfig], metadata: &[CloudDevice]) -> Vec<CloudDevice> {
    configs
        .iter()
        .map(|config| explicit_cloud_device(config, metadata))
        .collect()
}

fn explicit_cloud_device(config: &DeviceConfig, metadata: &[CloudDevice]) -> CloudDevice {
    let mut device = metadata
        .iter()
        .find(|device| device.id.as_deref() == Some(config.id.as_str()))
        .cloned()
        .unwrap_or_else(|| config_cloud_device(config));
    device.id = Some(config.id.clone());
    device
}

fn config_cloud_device(config: &DeviceConfig) -> CloudDevice {
    CloudDevice {
        id: Some(config.id.clone()),
        online: Some(true),
        ..CloudDevice::default()
    }
}

pub(crate) async fn cloud_mqtt_startup(
    cloud: Option<&CloudSession>,
    endpoint: &MqttEndpoint,
    device_ids: &[String],
) -> Result<Option<CloudMqttStartup>> {
    if device_ids.is_empty() {
        return Ok(None);
    }

    let cloud = cloud.with_context(|| {
        "cloud MQTT devices require a Bambu Cloud token; run `bambu-overlay login` or configure the device as --local-device"
    })?;
    let user_id = cloud_mqtt_user_id(cloud).await?;
    Ok(Some(CloudMqttStartup {
        host: endpoint.host.clone(),
        port: endpoint.port,
        user_id,
        access_token: cloud.access_token.clone(),
        device_ids: device_ids.to_vec(),
    }))
}

pub(crate) fn cloud_mqtt_device_ids(
    catalog_devices: &[CloudDevice],
    local_ids: &HashSet<String>,
) -> Vec<String> {
    catalog_devices
        .iter()
        .filter_map(|device| device.id.as_deref())
        .filter(|device_id| !local_ids.contains(*device_id))
        .map(str::to_owned)
        .collect()
}

pub(crate) fn start_cloud_mqtt(runtime: MqttRuntime, startup: Option<CloudMqttStartup>) {
    let Some(startup) = startup else {
        return;
    };

    let mqtt_status = runtime.clone();
    let supervisor = tokio::spawn(supervise_cloud(
        runtime,
        startup.host,
        startup.port,
        startup.user_id,
        startup.access_token,
        startup.device_ids,
    ));
    tokio::spawn(async move {
        match supervisor.await {
            Ok(()) => {
                warn!("MQTT supervisor exited unexpectedly");
                mqtt_status
                    .set_cloud_error("MQTT supervisor exited unexpectedly")
                    .await;
            }
            Err(error) => {
                error!(%error, "MQTT supervisor task failed");
                mqtt_status
                    .set_cloud_error(format!("MQTT supervisor task failed: {error}"))
                    .await;
            }
        }
    });
}

async fn cloud_mqtt_user_id(cloud: &CloudSession) -> Result<String> {
    let preference = cloud
        .client
        .user_preference(&cloud.access_token)
        .await
        .context("could not fetch MQTT user id from user preference")?;
    preference
        .mqtt_user_id()
        .context("could not derive MQTT user id from user preference")
}

#[cfg(test)]
mod tests {
    use super::{bound_cloud_devices, cloud_devices, cloud_mqtt_startup, explicit_cloud_devices};
    use crate::{bambu::CloudDevice, config::DeviceConfig, local::MqttEndpoint};

    fn mqtt_endpoint(value: &str) -> MqttEndpoint {
        value.parse().expect("MQTT endpoint should parse")
    }

    #[tokio::test]
    async fn cloud_mqtt_startup_skips_when_no_cloud_devices_exist() {
        let startup = cloud_mqtt_startup(None, &mqtt_endpoint("mqtt.example.test"), &[])
            .await
            .unwrap();

        assert!(startup.is_none());
    }

    #[tokio::test]
    async fn cloud_mqtt_startup_requires_cloud_session_for_cloud_devices() {
        let error = match cloud_mqtt_startup(
            None,
            &mqtt_endpoint("mqtt.example.test"),
            &["printer-a".to_owned()],
        )
        .await
        {
            Ok(_) => panic!("cloud MQTT startup should require a cloud session"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("Bambu Cloud token"));
    }

    #[tokio::test]
    async fn explicit_cloud_devices_do_not_require_cloud_session_for_catalog() {
        let devices = cloud_devices(
            None,
            &[DeviceConfig {
                id: "printer-a".to_owned(),
            }],
            false,
        )
        .await
        .unwrap();

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id.as_deref(), Some("printer-a"));
        assert_eq!(devices[0].access_code, None);
    }

    #[tokio::test]
    async fn cloud_devices_without_session_returns_empty_catalog() {
        let devices = cloud_devices(None, &[], false).await.unwrap();

        assert!(devices.is_empty());
    }

    #[tokio::test]
    async fn cloud_metadata_requires_cloud_session() {
        let error = bound_cloud_devices(None).await.unwrap_err();

        assert!(error.to_string().contains("/bind metadata"));
        assert!(error.to_string().contains("Bambu Cloud token"));
    }

    #[test]
    fn explicit_cloud_devices_can_use_fetched_metadata_without_enumerating_extra_devices() {
        let devices = explicit_cloud_devices(
            &[DeviceConfig {
                id: "printer-a".to_owned(),
            }],
            &[
                CloudDevice {
                    id: Some("printer-a".to_owned()),
                    name: Some("Office".to_owned()),
                    access_code: Some("12345678".to_owned()),
                    ..CloudDevice::default()
                },
                CloudDevice {
                    id: Some("printer-b".to_owned()),
                    name: Some("Garage".to_owned()),
                    access_code: Some("87654321".to_owned()),
                    ..CloudDevice::default()
                },
            ],
        );

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id.as_deref(), Some("printer-a"));
        assert_eq!(devices[0].name.as_deref(), Some("Office"));
        assert_eq!(devices[0].access_code.as_deref(), Some("12345678"));
    }
}
