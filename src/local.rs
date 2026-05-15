use std::{fmt, str::FromStr, time::Duration};

use anyhow::{Context, Result};
use tokio::net::TcpStream;

use crate::{
    bambu::{CloudDevice, MQTT_PORT},
    device_tls,
};

const LOCAL_MQTT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

pub type MqttEndpoint = Endpoint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDevice {
    pub id: String,
    pub host: String,
    pub mqtt_port: u16,
    pub access_code: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDeviceConfig {
    pub host: String,
    pub mqtt_port: u16,
    pub access_code: Option<String>,
    pub name: Option<String>,
}

impl LocalDevice {
    pub fn cloud_device(&self) -> CloudDevice {
        CloudDevice {
            id: Some(self.id.clone()),
            name: self.name.clone(),
            online: Some(true),
            access_code: self.access_code.clone(),
            ..CloudDevice::default()
        }
    }

    pub fn merge_cloud_metadata(&mut self, device: &CloudDevice) {
        if self.name.is_none() {
            self.name = device.name.clone();
        }
        if self.access_code.is_none() {
            self.access_code = device.access_code.clone();
        }
    }
}

impl LocalDeviceConfig {
    pub fn into_device(self, id: String) -> LocalDevice {
        LocalDevice {
            id,
            host: self.host,
            mqtt_port: self.mqtt_port,
            access_code: self.access_code,
            name: self.name,
        }
    }

    pub fn mqtt_endpoint(&self) -> Endpoint {
        Endpoint::new(self.host.clone(), self.mqtt_port)
    }
}

impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    pub fn parse_with_default(
        value: &str,
        label: &str,
        default_port: u16,
    ) -> std::result::Result<Self, String> {
        let value = value.trim();
        parse_endpoint(value, value, label, default_port)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudDeviceConfig {
    pub id: String,
    pub access_code: Option<String>,
    pub name: Option<String>,
}

impl CloudDeviceConfig {
    pub fn cloud_device(&self) -> CloudDevice {
        CloudDevice {
            id: Some(self.id.clone()),
            name: self.name.clone(),
            online: Some(true),
            access_code: self.access_code.clone(),
            ..CloudDevice::default()
        }
    }
}

impl FromStr for CloudDeviceConfig {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        parse_cloud_device(value)
    }
}

impl FromStr for Endpoint {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Endpoint::parse_with_default(value, "MQTT endpoint", MQTT_PORT)
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(formatter, "[{}]:{}", self.host, self.port)
        } else {
            write!(formatter, "{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for LocalDevice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(formatter, "{}=[{}]:{}", self.id, self.host, self.mqtt_port)
        } else {
            write!(formatter, "{}={}:{}", self.id, self.host, self.mqtt_port)
        }
    }
}

impl fmt::Display for CloudDeviceConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.id)
    }
}

impl FromStr for LocalDeviceConfig {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        parse_local_device(value)
    }
}

pub async fn infer_local_device_id(device: &LocalDeviceConfig) -> Result<String> {
    let endpoint = device.mqtt_endpoint();
    let address = endpoint.to_string();
    let tcp = tokio::time::timeout(
        LOCAL_MQTT_PROBE_TIMEOUT,
        TcpStream::connect((device.host.as_str(), device.mqtt_port)),
    )
    .await
    .with_context(|| format!("timed out probing local MQTT TLS at {address}"))?
    .with_context(|| format!("failed to connect to local MQTT TLS at {address}"))?;

    let tls = device_tls::tokio_connector()?;
    let socket = tokio::time::timeout(
        LOCAL_MQTT_PROBE_TIMEOUT,
        tls.connect(device.host.as_str(), tcp),
    )
    .await
    .with_context(|| format!("timed out handshaking local MQTT TLS at {address}"))?
    .with_context(|| format!("failed local MQTT TLS handshake at {address}"))?;

    device_tls::peer_device_id(&socket)
        .with_context(|| format!("local MQTT certificate at {address} did not include a device ID"))
}

fn parse_local_device(value: &str) -> std::result::Result<LocalDeviceConfig, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("local device must not be empty".to_owned());
    }
    if value.contains('=') {
        return Err(format!(
            "invalid local device `{value}`: DEVICE_ID= prefix is not supported; use HOST[:PORT][,ACCESS_CODE[,NAME]]"
        ));
    }

    let fields = value.splitn(3, ',').collect::<Vec<_>>();
    if fields.is_empty() {
        return Err(local_device_format_error(value));
    }

    let endpoint = parse_endpoint(fields[0].trim(), value, "local device", MQTT_PORT)?;
    let access_code = optional_field(&fields, 1);
    if access_code
        .as_deref()
        .is_some_and(|access_code| !access_code.is_ascii())
    {
        return Err(format!(
            "invalid local device `{value}`: access code must be ASCII"
        ));
    }
    let name = fields
        .get(2)
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned);

    Ok(LocalDeviceConfig {
        host: endpoint.host,
        mqtt_port: endpoint.port,
        access_code,
        name,
    })
}

fn local_device_format_error(value: &str) -> String {
    format!("invalid local device `{value}`: expected HOST[:PORT][,ACCESS_CODE[,NAME]]")
}

fn parse_cloud_device(value: &str) -> std::result::Result<CloudDeviceConfig, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("cloud device must not be empty".to_owned());
    }

    let fields = value.splitn(3, ',').collect::<Vec<_>>();
    let id = fields[0].trim();
    if id.is_empty() {
        return Err(format!(
            "invalid cloud device `{value}`: device id is empty"
        ));
    }

    Ok(CloudDeviceConfig {
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

fn parse_endpoint(
    endpoint: &str,
    value: &str,
    label: &str,
    default_port: u16,
) -> std::result::Result<Endpoint, String> {
    if endpoint.is_empty() {
        return Err(format!("invalid {label} `{value}`: host is empty"));
    }
    let (host, port) = split_host_port(endpoint, value, label)?;
    let host = host.trim();
    if host.is_empty() {
        return Err(format!("invalid {label} `{value}`: host is empty"));
    }
    let port = port
        .map(|port| parse_port(port, value, label))
        .transpose()?
        .unwrap_or(default_port);
    Ok(Endpoint::new(host, port))
}

fn split_host_port<'a>(
    endpoint: &'a str,
    value: &str,
    label: &str,
) -> std::result::Result<(&'a str, Option<&'a str>), String> {
    if let Some(rest) = endpoint.strip_prefix('[') {
        let Some((host, suffix)) = rest.split_once(']') else {
            return Err(format!("invalid {label} `{value}`"));
        };

        let port = match suffix.strip_prefix(':') {
            Some(port) => Some(port),
            None if suffix.is_empty() => None,
            _ => return Err(format!("invalid {label} `{value}`")),
        };
        return Ok((host, port));
    }

    if endpoint.matches(':').count() == 1 {
        let (host, port) = endpoint
            .split_once(':')
            .expect("single colon should split endpoint");
        return Ok((host, Some(port)));
    }

    Ok((endpoint, None))
}

fn parse_port(port: &str, value: &str, label: &str) -> std::result::Result<u16, String> {
    let port = port.trim();
    if port.is_empty() {
        return Err(format!("invalid {label} `{value}`: port is empty"));
    }
    port.parse::<u16>()
        .map_err(|_| format!("invalid {label} `{value}`: expected a valid port"))
}

#[cfg(test)]
mod tests {
    use super::LocalDeviceConfig;

    fn local_device_config(value: &str) -> LocalDeviceConfig {
        value.parse().expect("local device config should parse")
    }

    #[test]
    fn local_device_parser_accepts_default_mqtt_port() {
        let device = local_device_config("192.168.1.50,12345678,Office X1");

        assert_eq!(device.host, "192.168.1.50");
        assert_eq!(device.mqtt_port, 8883);
        assert_eq!(device.access_code.as_deref(), Some("12345678"));
        assert_eq!(device.name.as_deref(), Some("Office X1"));
    }

    #[test]
    fn local_device_parser_accepts_custom_port() {
        let device = local_device_config("printer.local:18883,12345678");

        assert_eq!(device.host, "printer.local");
        assert_eq!(device.mqtt_port, 18883);
        assert_eq!(device.name, None);
    }

    #[test]
    fn local_device_parser_accepts_missing_access_code() {
        let device = local_device_config("printer.local");

        assert_eq!(device.host, "printer.local");
        assert_eq!(device.access_code, None);
        assert_eq!(device.name, None);

        let device = local_device_config("printer.local,,Office X1");

        assert_eq!(device.access_code, None);
        assert_eq!(device.name.as_deref(), Some("Office X1"));
    }

    #[test]
    fn local_device_parser_accepts_bracketed_ipv6() {
        let device = local_device_config("[fe80::1]:18883,12345678");

        assert_eq!(device.host, "fe80::1");
        assert_eq!(device.mqtt_port, 18883);
    }

    #[test]
    fn local_device_parser_rejects_device_id_prefix() {
        let error = "printer-a=printer.local:18883,12345678"
            .parse::<LocalDeviceConfig>()
            .unwrap_err();

        assert!(error.contains("DEVICE_ID= prefix is not supported"));
    }

    #[test]
    fn local_device_config_accepts_host_only_form() {
        let device = local_device_config("printer.local:18883,12345678,Office X1");

        assert_eq!(device.host, "printer.local");
        assert_eq!(device.mqtt_port, 18883);
        assert_eq!(device.access_code.as_deref(), Some("12345678"));
        assert_eq!(device.name.as_deref(), Some("Office X1"));
    }

    #[test]
    fn cloud_device_parser_accepts_id_only_or_metadata() {
        let device: super::CloudDeviceConfig = "printer-a".parse().unwrap();
        assert_eq!(device.id, "printer-a");
        assert_eq!(device.access_code, None);

        let device: super::CloudDeviceConfig = "printer-a,12345678,Office X1".parse().unwrap();
        assert_eq!(device.access_code.as_deref(), Some("12345678"));
        assert_eq!(device.name.as_deref(), Some("Office X1"));
    }

    #[test]
    fn mqtt_endpoint_parser_defaults_to_port_8883() {
        let endpoint: super::MqttEndpoint = "us.mqtt.bambulab.com".parse().unwrap();

        assert_eq!(endpoint.host, "us.mqtt.bambulab.com");
        assert_eq!(endpoint.port, 8883);
        assert_eq!(endpoint.to_string(), "us.mqtt.bambulab.com:8883");
    }

    #[test]
    fn mqtt_endpoint_parser_accepts_custom_port_and_ipv6() {
        let endpoint: super::MqttEndpoint = "mqtt.example.test:18883".parse().unwrap();

        assert_eq!(endpoint.host, "mqtt.example.test");
        assert_eq!(endpoint.port, 18883);

        let endpoint: super::MqttEndpoint = "[fe80::1]:18883".parse().unwrap();

        assert_eq!(endpoint.host, "fe80::1");
        assert_eq!(endpoint.port, 18883);
        assert_eq!(endpoint.to_string(), "[fe80::1]:18883");
    }

    #[test]
    fn endpoint_parser_accepts_a_custom_default_port() {
        let endpoint =
            super::Endpoint::parse_with_default("127.0.0.1", "bind address", 8765).unwrap();

        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 8765);
        assert_eq!(endpoint.to_string(), "127.0.0.1:8765");
    }
}
