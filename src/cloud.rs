use std::collections::HashSet;

use anyhow::{Context, Result};
use tracing::{error, warn};

use crate::{
    bambu::{BambuClient, CloudDevice},
    devices::DeviceConfig,
    local::MqttEndpoint,
    mqtt::{supervise, MqttRuntime},
};

#[derive(Clone)]
pub struct CloudSession {
    pub client: BambuClient,
    pub access_token: String,
}

#[derive(Debug)]
pub(crate) struct CloudDevices {
    pub(crate) catalog: Vec<CloudDevice>,
    pub(crate) metadata: Vec<CloudDevice>,
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
    no_cloud_enum: bool,
) -> Result<CloudDevices> {
    let bound = if enumerate_cloud_catalog(no_cloud_enum) {
        bound_cloud_devices(cloud).await?
    } else {
        Vec::new()
    };
    let explicit = explicit_cloud_devices(configs, &bound, no_cloud_enum)?;
    let catalog = merge_cloud_catalog(bound, explicit);

    Ok(CloudDevices {
        metadata: catalog.clone(),
        catalog,
    })
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
    let supervisor = tokio::spawn(supervise(
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
                    .set_error("MQTT supervisor exited unexpectedly")
                    .await;
            }
            Err(error) => {
                error!(%error, "MQTT supervisor task failed");
                mqtt_status
                    .set_error(format!("MQTT supervisor task failed: {error}"))
                    .await;
            }
        }
    });
}

fn explicit_cloud_devices(
    configs: &[DeviceConfig],
    bound: &[CloudDevice],
    no_cloud_enum: bool,
) -> Result<Vec<CloudDevice>> {
    configs
        .iter()
        .map(|config| explicit_cloud_device(config, bound, no_cloud_enum))
        .collect()
}

fn explicit_cloud_device(
    config: &DeviceConfig,
    bound: &[CloudDevice],
    no_cloud_enum: bool,
) -> Result<CloudDevice> {
    let bound = bound
        .iter()
        .find(|device| device.id.as_deref() == Some(config.id.as_str()));
    let mut device = bound.cloned().unwrap_or_else(|| config.cloud_device());
    device.id = Some(config.id.clone());
    if let Some(name) = config.name.clone() {
        device.name = Some(name);
    }
    if let Some(access_code) = config.access_code.clone() {
        device.access_code = Some(access_code);
    }

    if device
        .access_code
        .as_deref()
        .is_none_or(|code| code.trim().is_empty())
    {
        if no_cloud_enum {
            anyhow::bail!(
                "--cloud-device `{}` is missing an access code; provide ACCESS_CODE or remove --no-cloud-enum to backfill from /bind",
                config.id
            );
        }
        if bound.is_none() {
            anyhow::bail!(
                "--cloud-device `{}` is missing an access code and was not returned by Bambu Cloud /bind",
                config.id
            );
        }
        anyhow::bail!(
            "--cloud-device `{}` was returned by Bambu Cloud /bind without dev_access_code",
            config.id
        );
    }

    Ok(device)
}

fn merge_cloud_catalog(
    mut bound: Vec<CloudDevice>,
    explicit: Vec<CloudDevice>,
) -> Vec<CloudDevice> {
    for explicit in explicit {
        if let Some(device_id) = explicit.id.as_deref() {
            if let Some(existing) = bound
                .iter_mut()
                .find(|device| device.id.as_deref() == Some(device_id))
            {
                *existing = explicit;
                continue;
            }
        }
        bound.push(explicit);
    }
    bound
}

async fn bound_cloud_devices(cloud: Option<&CloudSession>) -> Result<Vec<CloudDevice>> {
    let Some(cloud) = cloud else {
        return Ok(Vec::new());
    };
    let mut bound = cloud.client.bound_devices(&cloud.access_token).await?;
    for device in &mut bound.devices {
        device.status = Default::default();
    }
    Ok(bound.devices)
}

fn enumerate_cloud_catalog(no_cloud_enum: bool) -> bool {
    !no_cloud_enum
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
    use super::{
        cloud_devices, cloud_mqtt_startup, enumerate_cloud_catalog, explicit_cloud_devices,
        merge_cloud_catalog,
    };
    use crate::{bambu::CloudDevice, devices::DeviceConfig, local::MqttEndpoint};

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
                access_code: Some("12345678".to_owned()),
                name: Some("Office".to_owned()),
            }],
            true,
        )
        .await
        .unwrap();

        assert_eq!(devices.catalog.len(), 1);
        assert_eq!(devices.catalog[0].id.as_deref(), Some("printer-a"));
        assert_eq!(devices.catalog[0].access_code.as_deref(), Some("12345678"));
        assert_eq!(devices.metadata.len(), 1);
        assert_eq!(devices.metadata[0].id.as_deref(), Some("printer-a"));
    }

    #[test]
    fn cloud_catalog_enumeration_only_depends_on_no_cloud_enum() {
        assert!(enumerate_cloud_catalog(false));
        assert!(!enumerate_cloud_catalog(true));
    }

    #[tokio::test]
    async fn cloud_device_without_access_code_errors_when_cloud_enum_is_disabled() {
        let error = cloud_devices(
            None,
            &[DeviceConfig {
                id: "printer-a".to_owned(),
                access_code: None,
                name: None,
            }],
            true,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("--cloud-device `printer-a`"));
        assert!(error.to_string().contains("missing an access code"));
        assert!(error.to_string().contains("--no-cloud-enum"));
    }

    #[tokio::test]
    async fn cloud_device_without_access_code_errors_when_not_found_by_cloud_enum() {
        let error = cloud_devices(
            None,
            &[DeviceConfig {
                id: "printer-a".to_owned(),
                access_code: None,
                name: None,
            }],
            false,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("--cloud-device `printer-a`"));
        assert!(error
            .to_string()
            .contains("not returned by Bambu Cloud /bind"));
    }

    #[test]
    fn cloud_device_without_access_code_can_be_backfilled_from_cloud_enum() {
        let devices = explicit_cloud_devices(
            &[DeviceConfig {
                id: "printer-a".to_owned(),
                access_code: None,
                name: None,
            }],
            &[CloudDevice {
                id: Some("printer-a".to_owned()),
                name: Some("Office".to_owned()),
                online: Some(false),
                access_code: Some("12345678".to_owned()),
                ..CloudDevice::default()
            }],
            false,
        )
        .unwrap();

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id.as_deref(), Some("printer-a"));
        assert_eq!(devices[0].name.as_deref(), Some("Office"));
        assert_eq!(devices[0].online, Some(false));
        assert_eq!(devices[0].access_code.as_deref(), Some("12345678"));
    }

    #[test]
    fn explicit_cloud_devices_override_enumerated_cloud_devices() {
        let devices = merge_cloud_catalog(
            vec![
                CloudDevice {
                    id: Some("printer-a".to_owned()),
                    name: Some("Enumerated Office".to_owned()),
                    access_code: Some("11111111".to_owned()),
                    ..CloudDevice::default()
                },
                CloudDevice {
                    id: Some("printer-b".to_owned()),
                    name: Some("Garage".to_owned()),
                    access_code: Some("22222222".to_owned()),
                    ..CloudDevice::default()
                },
            ],
            vec![
                CloudDevice {
                    id: Some("printer-a".to_owned()),
                    name: Some("Explicit Office".to_owned()),
                    access_code: Some("33333333".to_owned()),
                    ..CloudDevice::default()
                },
                CloudDevice {
                    id: Some("printer-c".to_owned()),
                    name: Some("Lab".to_owned()),
                    access_code: Some("44444444".to_owned()),
                    ..CloudDevice::default()
                },
            ],
        );

        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].id.as_deref(), Some("printer-a"));
        assert_eq!(devices[0].name.as_deref(), Some("Explicit Office"));
        assert_eq!(devices[0].access_code.as_deref(), Some("33333333"));
        assert_eq!(devices[1].id.as_deref(), Some("printer-b"));
        assert_eq!(devices[2].id.as_deref(), Some("printer-c"));
    }

    #[tokio::test]
    async fn cloud_devices_without_session_returns_empty_catalog() {
        let devices = cloud_devices(None, &[], false).await.unwrap();

        assert!(devices.catalog.is_empty());
        assert!(devices.metadata.is_empty());
    }
}
