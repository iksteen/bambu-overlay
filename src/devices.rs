use std::{
    collections::{HashMap, HashSet},
    fmt,
    str::FromStr,
};

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::{
    bambu::CloudDevice,
    cloud::{cloud_devices as resolve_cloud_devices, cloud_mqtt_device_ids, CloudSession},
    local::{infer_local_device_id, LocalDevice, LocalDeviceConfig},
    video::{infer_video_device_id, probe_video_endpoint, VideoEndpoint, DEFAULT_VIDEO_PORT},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConfig {
    pub id: String,
    pub access_code: Option<String>,
    pub name: Option<String>,
}

impl DeviceConfig {
    pub(crate) fn cloud_device(&self) -> CloudDevice {
        CloudDevice {
            id: Some(self.id.clone()),
            name: self.name.clone(),
            online: Some(true),
            access_code: self.access_code.clone(),
            ..CloudDevice::default()
        }
    }
}

impl FromStr for DeviceConfig {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        parse_device_config(value)
    }
}

impl fmt::Display for DeviceConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.id)
    }
}

pub(crate) struct ResolvedDevices {
    pub(crate) catalog: Vec<CloudDevice>,
    pub(crate) local: Vec<LocalDevice>,
    pub(crate) local_ids: HashSet<String>,
    pub(crate) cloud_mqtt_ids: Vec<String>,
}

pub(crate) struct ResolvedVideoEndpoints {
    pub(crate) endpoints: Vec<VideoEndpoint>,
    pub(crate) probed_device_ids: HashSet<String>,
    pub(crate) endpoint_map: HashMap<String, VideoEndpoint>,
}

pub(crate) async fn resolve_devices(
    cloud: Option<&CloudSession>,
    cloud_configs: &[DeviceConfig],
    local_configs: &[LocalDeviceConfig],
    no_cloud_enum: bool,
) -> Result<ResolvedDevices> {
    let cloud_devices = resolve_cloud_devices(cloud, cloud_configs, no_cloud_enum).await?;
    let mut local = resolve_local_devices(local_configs).await?;
    ensure_unique_local_device_ids(&local)?;
    merge_local_metadata(&mut local, &cloud_devices.metadata);
    ensure_local_names(&mut local);
    ensure_local_access_codes(&local)?;

    let local_ids = local_device_ids(&local);
    let catalog = catalog_devices(cloud_devices.catalog, &local, &local_ids);
    if catalog.is_empty() {
        anyhow::bail!(
            "no devices configured; run `bambu-overlay login`, set --cloud-device, or set --local-device"
        );
    }

    let cloud_mqtt_ids = cloud_mqtt_device_ids(&catalog, &local_ids);
    Ok(ResolvedDevices {
        catalog,
        local,
        local_ids,
        cloud_mqtt_ids,
    })
}

pub(crate) async fn resolve_video_endpoints(
    explicit: &[VideoEndpoint],
    devices: &ResolvedDevices,
) -> Result<ResolvedVideoEndpoints> {
    let catalog_ids = catalog_device_ids(&devices.catalog);
    let mut endpoints = Vec::with_capacity(explicit.len() + devices.local.len());
    let mut probed_device_ids = HashSet::new();
    let mut endpoint_map = HashMap::new();
    let mut candidates = Vec::new();
    let mut probes = tokio::task::JoinSet::new();

    for endpoint in explicit {
        let device_id = infer_video_device_id(endpoint).await.with_context(|| {
            format!("could not infer device ID for --video-device `{endpoint}`")
        })?;
        ensure_video_device_exists(endpoint, &device_id, &catalog_ids)?;
        info!(
            device_id = %device_id,
            endpoint = %endpoint,
            "validated explicit local video endpoint"
        );
        endpoints.push(endpoint.clone());
        candidates.push(endpoint.clone());
        probed_device_ids.insert(device_id.clone());
        endpoint_map.insert(device_id, endpoint.clone());
    }

    for device in &devices.local {
        let endpoint = local_video_endpoint(device);
        if candidates.iter().any(|candidate| candidate == &endpoint) {
            continue;
        }

        candidates.push(endpoint.clone());
        let device_id = device.id.clone();
        probes.spawn(async move {
            let result = probe_video_endpoint(&device_id, &endpoint).await;
            (device_id, endpoint, result)
        });
    }

    while let Some(result) = probes.join_next().await {
        match result {
            Ok((device_id, endpoint, Ok(()))) => {
                info!(
                    device_id = %device_id,
                    endpoint = %endpoint,
                    "auto-enabled local video endpoint"
                );
                probed_device_ids.insert(device_id.clone());
                endpoint_map.insert(device_id, endpoint.clone());
                endpoints.push(endpoint);
            }
            Ok((device_id, endpoint, Err(error))) => {
                debug!(
                    device_id = %device_id,
                    endpoint = %endpoint,
                    error = %error,
                    "local video endpoint probe failed"
                );
            }
            Err(error) => {
                debug!(%error, "local video endpoint probe task failed");
            }
        }
    }

    Ok(ResolvedVideoEndpoints {
        endpoints,
        probed_device_ids,
        endpoint_map,
    })
}

async fn resolve_local_devices(configs: &[LocalDeviceConfig]) -> Result<Vec<LocalDevice>> {
    let mut devices = Vec::with_capacity(configs.len());
    for config in configs {
        let id = infer_local_device_id(config).await.with_context(|| {
            format!(
                "could not infer device ID for --local-device `{}`",
                config.mqtt_endpoint()
            )
        })?;
        info!(
            device_id = %id,
            endpoint = %config.mqtt_endpoint(),
            "inferred local device ID from MQTT certificate"
        );
        devices.push(config.clone().into_device(id));
    }
    Ok(devices)
}

fn ensure_unique_local_device_ids(local_devices: &[LocalDevice]) -> Result<()> {
    let mut seen = HashSet::new();
    for device in local_devices {
        if !seen.insert(device.id.as_str()) {
            anyhow::bail!(
                "--local-device resolves duplicate device id `{}`",
                device.id
            );
        }
    }
    Ok(())
}

fn merge_local_metadata(local_devices: &mut [LocalDevice], cloud_devices: &[CloudDevice]) {
    for local in local_devices {
        if let Some(cloud) = cloud_devices
            .iter()
            .find(|cloud| cloud.id.as_deref() == Some(local.id.as_str()))
        {
            local.merge_cloud_metadata(cloud);
        }
    }
}

fn ensure_local_names(local_devices: &mut [LocalDevice]) {
    for device in local_devices {
        if device
            .name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
        {
            device.name = Some(device.id.clone());
        }
    }
}

fn ensure_local_access_codes(local_devices: &[LocalDevice]) -> Result<()> {
    for device in local_devices {
        if device
            .access_code
            .as_deref()
            .is_none_or(|code| code.trim().is_empty())
        {
            anyhow::bail!(
                "--local-device `{}` is missing an access code; provide ACCESS_CODE or cloud metadata that exposes dev_access_code",
                device.id
            );
        }
    }
    Ok(())
}

fn local_device_ids(local_devices: &[LocalDevice]) -> HashSet<String> {
    local_devices
        .iter()
        .map(|device| device.id.clone())
        .collect()
}

fn catalog_devices(
    cloud_devices: Vec<CloudDevice>,
    local_devices: &[LocalDevice],
    local_ids: &HashSet<String>,
) -> Vec<CloudDevice> {
    let mut devices = cloud_devices
        .into_iter()
        .filter(|device| {
            device
                .id
                .as_deref()
                .is_none_or(|device_id| !local_ids.contains(device_id))
        })
        .collect::<Vec<_>>();
    devices.extend(local_devices.iter().map(LocalDevice::cloud_device));
    devices
}

fn catalog_device_ids(devices: &[CloudDevice]) -> HashSet<String> {
    devices
        .iter()
        .filter_map(|device| device.id.clone())
        .collect()
}

fn ensure_video_device_exists(
    endpoint: &VideoEndpoint,
    device_id: &str,
    catalog_device_ids: &HashSet<String>,
) -> Result<()> {
    if !catalog_device_ids.contains(device_id) {
        anyhow::bail!(
            "--video-device `{endpoint}` is for device `{device_id}`, but no matching cloud or local device is configured"
        );
    }
    Ok(())
}

fn local_video_endpoint(device: &LocalDevice) -> VideoEndpoint {
    VideoEndpoint::new(device.host.clone(), DEFAULT_VIDEO_PORT)
}

fn parse_device_config(value: &str) -> std::result::Result<DeviceConfig, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("device config must not be empty".to_owned());
    }

    let fields = value.splitn(3, ',').collect::<Vec<_>>();
    let id = fields[0].trim();
    if id.is_empty() {
        return Err(format!(
            "invalid device config `{value}`: device id is empty"
        ));
    }

    Ok(DeviceConfig {
        id: id.to_owned(),
        access_code: optional_field(&fields, 1),
        name: optional_field(&fields, 2),
    })
}

fn optional_field(fields: &[&str], index: usize) -> Option<String> {
    fields
        .get(index)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        catalog_device_ids, catalog_devices, ensure_local_access_codes, ensure_local_names,
        ensure_video_device_exists, local_device_ids, local_video_endpoint, merge_local_metadata,
    };
    use crate::{
        bambu::CloudDevice,
        local::{LocalDevice, LocalDeviceConfig},
        video::VideoEndpoint,
    };

    fn local(id: &str, value: &str) -> LocalDevice {
        let config: LocalDeviceConfig = value.parse().expect("local device should parse");
        config.into_device(id.to_owned())
    }

    fn endpoint(value: &str) -> VideoEndpoint {
        value.parse().expect("video endpoint should parse")
    }

    #[test]
    fn device_config_parser_accepts_id_only_or_metadata() {
        let device: super::DeviceConfig = "printer-a".parse().unwrap();
        assert_eq!(device.id, "printer-a");
        assert_eq!(device.access_code, None);

        let device: super::DeviceConfig = "printer-a,12345678,Office X1".parse().unwrap();
        assert_eq!(device.access_code.as_deref(), Some("12345678"));
        assert_eq!(device.name.as_deref(), Some("Office X1"));
    }

    #[test]
    fn catalog_uses_local_device_when_ids_overlap() {
        let local_devices = vec![local("printer-a", "192.168.1.50,12345678,Office")];
        let local_ids = local_device_ids(&local_devices);
        let catalog = catalog_devices(
            vec![
                CloudDevice {
                    id: Some("printer-a".to_owned()),
                    name: Some("Cloud Office".to_owned()),
                    access_code: Some("87654321".to_owned()),
                    ..CloudDevice::default()
                },
                CloudDevice {
                    id: Some("printer-b".to_owned()),
                    name: Some("Garage".to_owned()),
                    ..CloudDevice::default()
                },
            ],
            &local_devices,
            &local_ids,
        );

        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].id.as_deref(), Some("printer-b"));
        assert_eq!(catalog[1].id.as_deref(), Some("printer-a"));
        assert_eq!(catalog[1].access_code.as_deref(), Some("12345678"));
    }

    #[test]
    fn catalog_device_ids_ignores_devices_without_ids() {
        let ids = catalog_device_ids(&[
            CloudDevice {
                id: Some("printer-a".to_owned()),
                ..CloudDevice::default()
            },
            CloudDevice::default(),
        ]);

        assert_eq!(ids, HashSet::from(["printer-a".to_owned()]));
    }

    #[test]
    fn local_video_endpoint_uses_host_and_default_port() {
        let device = local("printer-a", "192.168.1.50,12345678,Office");

        assert_eq!(local_video_endpoint(&device), endpoint("192.168.1.50:6000"));
    }

    #[test]
    fn missing_local_access_code_is_backfilled_from_cloud_metadata() {
        let mut local_devices = vec![local("printer-a", "192.168.1.50,,Office")];
        let cloud_devices = vec![CloudDevice {
            id: Some("printer-a".to_owned()),
            access_code: Some("12345678".to_owned()),
            ..CloudDevice::default()
        }];

        merge_local_metadata(&mut local_devices, &cloud_devices);

        ensure_local_access_codes(&local_devices).unwrap();
        assert_eq!(local_devices[0].access_code.as_deref(), Some("12345678"));
    }

    #[test]
    fn local_device_name_defaults_to_device_id_when_missing() {
        let mut local_devices = vec![local("printer-a", "192.168.1.50,12345678")];

        ensure_local_names(&mut local_devices);

        assert_eq!(local_devices[0].name.as_deref(), Some("printer-a"));
    }

    #[test]
    fn local_device_name_keeps_explicit_or_cloud_name() {
        let mut local_devices = vec![
            local("printer-a", "192.168.1.50,12345678,Office"),
            local("printer-b", "192.168.1.51,87654321"),
        ];
        let cloud_devices = vec![CloudDevice {
            id: Some("printer-b".to_owned()),
            name: Some("Garage".to_owned()),
            ..CloudDevice::default()
        }];

        merge_local_metadata(&mut local_devices, &cloud_devices);
        ensure_local_names(&mut local_devices);

        assert_eq!(local_devices[0].name.as_deref(), Some("Office"));
        assert_eq!(local_devices[1].name.as_deref(), Some("Garage"));
    }

    #[test]
    fn missing_local_access_code_errors_when_not_backfilled() {
        let local_devices = vec![local("printer-a", "192.168.1.50")];

        let error = ensure_local_access_codes(&local_devices).unwrap_err();

        assert!(error.to_string().contains("printer-a"));
        assert!(error.to_string().contains("missing an access code"));
    }

    #[test]
    fn explicit_video_requires_matching_known_device() {
        let endpoint = endpoint("192.168.1.50");
        let error = ensure_video_device_exists(
            &endpoint,
            "printer-a",
            &HashSet::from(["printer-b".to_owned()]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("--video-device"));
        assert!(error.to_string().contains("printer-a"));
        assert!(error
            .to_string()
            .contains("no matching cloud or local device"));
    }
}
