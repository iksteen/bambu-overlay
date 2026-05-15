use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::{debug, info};

use crate::{
    bambu::{CloudDevice, PrinterStatus},
    cloud::{bound_cloud_devices, explicit_cloud_devices, CloudSession},
    local::{infer_local_device_id, Endpoint, LocalDevice, LocalEndpointArg},
    video::{infer_video_device_id, probe_video_endpoint, VideoEndpoint, DEFAULT_VIDEO_PORT},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DeviceSource {
    Cloud,
    Local,
}

#[derive(Debug, Clone)]
pub(crate) struct KnownDevice {
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) online: Option<bool>,
    pub(crate) access_code: Option<String>,
    pub(crate) status: PrinterStatus,
    pub(crate) source: DeviceSource,
}

impl KnownDevice {
    pub(crate) fn from_cloud(device: CloudDevice) -> Self {
        Self {
            id: device.id,
            name: device.name,
            online: device.online,
            access_code: device.access_code,
            status: device.status,
            source: DeviceSource::Cloud,
        }
    }

    pub(crate) fn from_local(device: &LocalDevice) -> Self {
        Self {
            id: Some(device.id.clone()),
            name: device.endpoint.name.clone(),
            online: Some(true),
            access_code: Some(device.endpoint.access_code.clone()),
            status: PrinterStatus::default(),
            source: DeviceSource::Local,
        }
    }

    pub(crate) fn has_access_code(&self) -> bool {
        self.access_code
            .as_deref()
            .is_some_and(|code| !code.trim().is_empty())
    }
}

pub(crate) struct ResolvedDevices {
    pub(crate) catalog: Vec<KnownDevice>,
    pub(crate) local: Vec<LocalDevice>,
    pub(crate) cloud_mqtt_ids: Vec<String>,
    explicit_video: Vec<(String, VideoEndpoint)>,
}

pub(crate) struct ResolvedVideoEndpoints {
    pub(crate) endpoints: Vec<VideoEndpoint>,
    pub(crate) probed_device_ids: HashSet<String>,
    pub(crate) endpoint_map: HashMap<String, VideoEndpoint>,
}

pub(crate) async fn resolve_devices(
    cloud: Option<&CloudSession>,
    cloud_configs: &[String],
    local_configs: &[LocalEndpointArg],
    video_endpoints: &[VideoEndpoint],
) -> Result<ResolvedDevices> {
    let explicit_video = resolve_explicit_video_endpoints(video_endpoints).await?;
    let local_args = infer_local_devices(local_configs).await?;
    ensure_unique_local_device_ids(&local_args)?;
    let enumerate_cloud_catalog =
        should_enumerate_cloud_catalog(cloud.is_some(), cloud_configs, &local_args);
    let cloud_devices = if enumerate_cloud_catalog {
        bound_cloud_devices(cloud).await?
    } else {
        explicit_cloud_devices(cloud_configs)
    };
    let mut bind_metadata = enumerate_cloud_catalog.then(|| cloud_devices.clone());
    let local =
        resolve_local_access(local_args, &explicit_video, cloud, &mut bind_metadata).await?;

    let local_ids = local_device_ids(&local);
    let mut catalog = catalog_devices(cloud_devices, &local, &local_ids);
    resolve_catalog_video_access(&mut catalog, &explicit_video, cloud, &mut bind_metadata).await?;
    let cloud_mqtt_ids = cloud_mqtt_device_ids(&catalog);
    if catalog.is_empty() {
        anyhow::bail!(
            "no devices configured; run `bambu-overlay login`, set --cloud-device, or set --local-device"
        );
    }

    Ok(ResolvedDevices {
        catalog,
        local,
        cloud_mqtt_ids,
        explicit_video,
    })
}

pub(crate) async fn resolve_video_endpoints(
    devices: &ResolvedDevices,
) -> Result<ResolvedVideoEndpoints> {
    let catalog_ids = catalog_device_ids(&devices.catalog);
    let mut endpoints = Vec::with_capacity(devices.explicit_video.len() + devices.local.len());
    let mut probed_device_ids = HashSet::new();
    let mut endpoint_map = HashMap::new();
    let mut candidates = Vec::new();
    let mut probes = tokio::task::JoinSet::new();

    for (device_id, endpoint) in &devices.explicit_video {
        ensure_video_device_exists(endpoint, device_id, &catalog_ids)?;
        info!(
            device_id = %device_id,
            endpoint = %endpoint,
            "validated explicit local video endpoint"
        );
        endpoints.push(endpoint.clone());
        candidates.push(endpoint.clone());
        probed_device_ids.insert(device_id.clone());
        endpoint_map.insert(device_id.clone(), endpoint.clone());
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

async fn infer_local_devices(
    configs: &[LocalEndpointArg],
) -> Result<Vec<(String, LocalEndpointArg)>> {
    let mut devices = Vec::with_capacity(configs.len());
    for config in configs {
        let id = infer_local_device_id(config).await.with_context(|| {
            format!(
                "could not infer device ID for --local-device `{}`",
                config.endpoint()
            )
        })?;
        info!(
            device_id = %id,
            endpoint = %config.endpoint(),
            "inferred local device ID from MQTT certificate"
        );
        devices.push((id, config.clone()));
    }
    Ok(devices)
}

async fn resolve_local_access(
    local_devices: Vec<(String, LocalEndpointArg)>,
    video_endpoints: &[(String, VideoEndpoint)],
    cloud: Option<&CloudSession>,
    bind_metadata: &mut Option<Vec<CloudDevice>>,
) -> Result<Vec<LocalDevice>> {
    let mut devices = Vec::with_capacity(local_devices.len());
    for device in local_devices {
        devices.push(
            resolve_local_device_access(device, video_endpoints, cloud, bind_metadata).await?,
        );
    }
    Ok(devices)
}

async fn resolve_local_device_access(
    device: (String, LocalEndpointArg),
    video_endpoints: &[(String, VideoEndpoint)],
    cloud: Option<&CloudSession>,
    bind_metadata: &mut Option<Vec<CloudDevice>>,
) -> Result<LocalDevice> {
    let (device_id, mut endpoint) = device;
    if !has_access_code(endpoint.access_code.as_deref()) {
        if let Some(video) = explicit_video_for_device(video_endpoints, &device_id) {
            endpoint.access_code = video.access_code().map(str::to_owned);
        }
    }
    if !has_access_code(endpoint.access_code.as_deref()) {
        if let Some(metadata) = bind_device(cloud, bind_metadata, &device_id).await? {
            endpoint.access_code = metadata.access_code;
            if !has_text(endpoint.name.as_deref()) {
                endpoint.name = metadata.name;
            }
        }
    }
    if !has_text(endpoint.name.as_deref()) {
        endpoint.name = Some(device_id.clone());
    }
    finalize_local_device((device_id, endpoint))
}

async fn resolve_explicit_video_endpoints(
    endpoints: &[VideoEndpoint],
) -> Result<Vec<(String, VideoEndpoint)>> {
    let mut resolved = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        let device_id = infer_video_device_id(endpoint).await.with_context(|| {
            format!("could not infer device ID for --video-device `{endpoint}`")
        })?;
        resolved.push((device_id, endpoint.clone()));
    }
    Ok(resolved)
}

fn should_enumerate_cloud_catalog(
    cloud_available: bool,
    cloud_configs: &[String],
    local_devices: &[(String, LocalEndpointArg)],
) -> bool {
    cloud_available && cloud_configs.is_empty() && local_devices.is_empty()
}

async fn resolve_catalog_video_access(
    catalog: &mut [KnownDevice],
    video_endpoints: &[(String, VideoEndpoint)],
    cloud: Option<&CloudSession>,
    bind_metadata: &mut Option<Vec<CloudDevice>>,
) -> Result<()> {
    for (device_id, video) in video_endpoints {
        let Some(device) = catalog
            .iter_mut()
            .find(|device| device.id.as_deref() == Some(device_id.as_str()))
        else {
            continue;
        };
        resolve_known_device_access(device, video, cloud, bind_metadata).await?;
    }
    Ok(())
}

async fn resolve_known_device_access(
    device: &mut KnownDevice,
    video: &VideoEndpoint,
    cloud: Option<&CloudSession>,
    bind_metadata: &mut Option<Vec<CloudDevice>>,
) -> Result<()> {
    if !device.has_access_code() {
        device.access_code = video.access_code().map(str::to_owned);
    }
    if !device.has_access_code() {
        if let Some(device_id) = device.id.as_deref() {
            if let Some(metadata) = bind_device(cloud, bind_metadata, device_id).await? {
                device.access_code = metadata.access_code;
                if !has_text(device.name.as_deref()) {
                    device.name = metadata.name;
                }
                device.online = device.online.or(metadata.online);
            }
        }
    }
    Ok(())
}

async fn bind_device(
    cloud: Option<&CloudSession>,
    bind_metadata: &mut Option<Vec<CloudDevice>>,
    device_id: &str,
) -> Result<Option<CloudDevice>> {
    if bind_metadata.is_none() {
        *bind_metadata = Some(bound_cloud_devices(cloud).await?);
    }
    Ok(bind_metadata
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|device| device.id.as_deref() == Some(device_id))
        .cloned())
}

fn explicit_video_for_device<'a>(
    video_endpoints: &'a [(String, VideoEndpoint)],
    device_id: &str,
) -> Option<&'a VideoEndpoint> {
    video_endpoints
        .iter()
        .find(|(video_device_id, _)| video_device_id == device_id)
        .map(|(_, endpoint)| endpoint)
}

fn has_access_code(access_code: Option<&str>) -> bool {
    has_text(access_code)
}

fn has_text(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn ensure_unique_local_device_ids(local_devices: &[(String, LocalEndpointArg)]) -> Result<()> {
    let mut seen = HashSet::new();
    for (device_id, _) in local_devices {
        if !seen.insert(device_id.as_str()) {
            anyhow::bail!(
                "--local-device resolves duplicate device id `{}`",
                device_id
            );
        }
    }
    Ok(())
}

fn finalize_local_device(device: (String, LocalEndpointArg)) -> Result<LocalDevice> {
    let (device_id, endpoint) = device;
    let access_code = endpoint
        .access_code
        .as_deref()
        .filter(|access_code| !access_code.trim().is_empty())
        .map(str::to_owned)
        .with_context(|| {
            format!(
                "--local-device `{}` is missing an access code; provide ACCESS_CODE or cloud metadata that exposes dev_access_code",
                device_id
            )
        })?;
    Ok(LocalDevice {
        id: device_id,
        endpoint: endpoint.into_endpoint(access_code),
    })
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
) -> Vec<KnownDevice> {
    let mut devices = cloud_devices
        .into_iter()
        .filter(|device| {
            device
                .id
                .as_deref()
                .is_none_or(|device_id| !local_ids.contains(device_id))
        })
        .map(KnownDevice::from_cloud)
        .collect::<Vec<_>>();
    devices.extend(local_devices.iter().map(KnownDevice::from_local));
    devices
}

fn catalog_device_ids(devices: &[KnownDevice]) -> HashSet<String> {
    devices
        .iter()
        .filter_map(|device| device.id.clone())
        .collect()
}

fn cloud_mqtt_device_ids(devices: &[KnownDevice]) -> Vec<String> {
    devices
        .iter()
        .filter(|device| device.source == DeviceSource::Cloud)
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
    VideoEndpoint::new(
        Endpoint::new(device.endpoint.host().to_owned(), DEFAULT_VIDEO_PORT),
        None,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        catalog_device_ids, catalog_devices, ensure_video_device_exists, finalize_local_device,
        local_device_ids, local_video_endpoint, resolve_catalog_video_access,
        resolve_local_device_access, should_enumerate_cloud_catalog, KnownDevice,
    };
    use crate::{
        bambu::CloudDevice,
        local::{LocalDevice, LocalEndpointArg},
        video::VideoEndpoint,
    };

    fn pending_local(id: &str, value: &str) -> (String, LocalEndpointArg) {
        let endpoint: LocalEndpointArg = value.parse().expect("local device should parse");
        (id.to_owned(), endpoint)
    }

    fn local(id: &str, value: &str) -> LocalDevice {
        finalize_local_device(pending_local(id, value)).expect("local device should be complete")
    }

    fn endpoint(value: &str) -> VideoEndpoint {
        value.parse().expect("video endpoint should parse")
    }

    fn explicit_video_endpoint(device_id: &str, value: &str) -> (String, VideoEndpoint) {
        (device_id.to_owned(), endpoint(value))
    }

    #[test]
    fn catalog_uses_local_device_when_ids_overlap() {
        let local_devices = vec![local("printer-a", "192.168.1.50,12345678,Office")];
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
            &local_device_ids(&local_devices),
        );

        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].id.as_deref(), Some("printer-b"));
        assert_eq!(catalog[1].id.as_deref(), Some("printer-a"));
        assert_eq!(catalog[1].access_code.as_deref(), Some("12345678"));
    }

    #[test]
    fn catalog_device_ids_ignores_devices_without_ids() {
        let ids = catalog_device_ids(&[
            super::KnownDevice::from_cloud(CloudDevice {
                id: Some("printer-a".to_owned()),
                ..CloudDevice::default()
            }),
            super::KnownDevice::from_cloud(CloudDevice::default()),
        ]);

        assert_eq!(ids, HashSet::from(["printer-a".to_owned()]));
    }

    #[test]
    fn local_video_endpoint_uses_host_and_default_port() {
        let device = local("printer-a", "192.168.1.50,12345678,Office");

        assert_eq!(local_video_endpoint(&device), endpoint("192.168.1.50:6000"));
    }

    #[tokio::test]
    async fn local_device_name_defaults_to_device_id_when_missing() {
        let mut bind_metadata = None;
        let device = resolve_local_device_access(
            pending_local("printer-a", "192.168.1.50,12345678"),
            &[],
            None,
            &mut bind_metadata,
        )
        .await
        .unwrap();

        assert_eq!(device.endpoint.name.as_deref(), Some("printer-a"));
    }

    #[tokio::test]
    async fn local_device_name_keeps_explicit_name() {
        let mut bind_metadata = None;
        let device = resolve_local_device_access(
            pending_local("printer-a", "192.168.1.50,12345678,Office"),
            &[],
            None,
            &mut bind_metadata,
        )
        .await
        .unwrap();

        assert_eq!(device.endpoint.name.as_deref(), Some("Office"));
    }

    #[tokio::test]
    async fn missing_local_access_code_errors_when_no_metadata_source_exists() {
        let local_device = pending_local("printer-a", "192.168.1.50");
        let mut bind_metadata = None;

        let error = resolve_local_device_access(local_device, &[], None, &mut bind_metadata)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("Bambu Cloud token"));
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

    #[test]
    fn cloud_catalog_enumeration_only_happens_when_no_devices_are_configured() {
        assert!(should_enumerate_cloud_catalog(true, &[], &[]));
        assert!(!should_enumerate_cloud_catalog(false, &[], &[]));
        assert!(!should_enumerate_cloud_catalog(
            true,
            &["printer-a".to_owned()],
            &[]
        ));
        assert!(!should_enumerate_cloud_catalog(
            true,
            &[],
            &[pending_local("printer-a", "192.168.1.50,12345678")]
        ));
    }

    #[tokio::test]
    async fn video_access_code_can_resolve_matching_local_and_catalog_devices() {
        let video = vec![explicit_video_endpoint(
            "printer-a",
            "192.168.1.50,12345678",
        )];
        let mut bind_metadata = None;
        let local_device = resolve_local_device_access(
            pending_local("printer-a", "192.168.1.50"),
            &video,
            None,
            &mut bind_metadata,
        )
        .await
        .unwrap();
        assert_eq!(local_device.endpoint.access_code.as_str(), "12345678");

        let mut catalog = vec![KnownDevice::from_cloud(CloudDevice {
            id: Some("printer-a".to_owned()),
            ..CloudDevice::default()
        })];
        resolve_catalog_video_access(&mut catalog, &video, None, &mut bind_metadata)
            .await
            .unwrap();
        assert_eq!(catalog[0].access_code.as_deref(), Some("12345678"));
    }

    #[tokio::test]
    async fn catalog_video_access_loads_bind_only_when_code_is_missing() {
        let video = vec![explicit_video_endpoint("printer-a", "192.168.1.50")];
        let mut catalog = vec![KnownDevice::from_cloud(CloudDevice {
            id: Some("printer-a".to_owned()),
            ..CloudDevice::default()
        })];
        let mut bind_metadata = None;

        let error = resolve_catalog_video_access(&mut catalog, &video, None, &mut bind_metadata)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("Bambu Cloud token"));
    }
}
