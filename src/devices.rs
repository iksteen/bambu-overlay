use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;

use crate::bambu::CloudDevice;

#[derive(Clone, Default)]
pub struct DeviceRegistry {
    order: Arc<Mutex<Vec<String>>>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn order_cloud_devices(&self, devices: Vec<CloudDevice>) -> Vec<CloudDevice> {
        let mut order = self.order.lock().await;
        for device_id in devices.iter().filter_map(cloud_device_id) {
            if !order.iter().any(|known| known == device_id) {
                order.push(device_id.to_owned());
            }
        }

        let positions = order
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect::<HashMap<_, _>>();
        drop(order);

        let mut indexed = devices.into_iter().enumerate().collect::<Vec<_>>();
        indexed.sort_by_key(|(index, device)| {
            (
                cloud_device_id(device)
                    .and_then(|id| positions.get(id).copied())
                    .unwrap_or(usize::MAX),
                *index,
            )
        });
        indexed.into_iter().map(|(_, device)| device).collect()
    }
}

fn cloud_device_id(device: &CloudDevice) -> Option<&str> {
    device
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::DeviceRegistry;
    use crate::bambu::CloudDevice;

    fn device(id: &str) -> CloudDevice {
        serde_json::from_value(json!({"dev_id": id})).expect("device should deserialize")
    }

    #[tokio::test]
    async fn keeps_first_seen_device_order_for_the_process() {
        let registry = DeviceRegistry::new();

        let first = registry
            .order_cloud_devices(vec![device("printer-b"), device("printer-a")])
            .await;
        let second = registry
            .order_cloud_devices(vec![device("printer-a"), device("printer-b")])
            .await;

        let first_ids = first
            .into_iter()
            .map(|device| device.id.expect("device id"))
            .collect::<Vec<_>>();
        let second_ids = second
            .into_iter()
            .map(|device| device.id.expect("device id"))
            .collect::<Vec<_>>();

        assert_eq!(first_ids, ["printer-b", "printer-a"]);
        assert_eq!(second_ids, ["printer-b", "printer-a"]);
    }

    #[tokio::test]
    async fn appends_new_devices_after_known_devices() {
        let registry = DeviceRegistry::new();
        registry
            .order_cloud_devices(vec![device("printer-b"), device("printer-a")])
            .await;

        let ordered = registry
            .order_cloud_devices(vec![
                device("printer-c"),
                device("printer-a"),
                device("printer-b"),
            ])
            .await
            .into_iter()
            .map(|device| device.id.expect("device id"))
            .collect::<Vec<_>>();

        assert_eq!(ordered, ["printer-b", "printer-a", "printer-c"]);
    }
}
