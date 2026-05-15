use std::{fmt, str::FromStr, time::Duration};

use anyhow::{Context, Result};
use tokio::net::TcpStream;

use crate::{bambu::MQTT_PORT, device_tls};

const LOCAL_MQTT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

pub type MqttEndpoint = Endpoint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEndpoint {
    pub endpoint: Endpoint,
    pub access_code: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEndpointArg {
    pub endpoint: Endpoint,
    pub access_code: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDevice {
    pub id: String,
    pub endpoint: LocalEndpoint,
}

impl LocalEndpoint {
    pub fn new(host: impl Into<String>, port: u16, access_code: impl Into<String>) -> Self {
        Self {
            endpoint: Endpoint::new(host, port),
            access_code: access_code.into(),
            name: None,
        }
    }

    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    pub fn host(&self) -> &str {
        self.endpoint.host.as_str()
    }

    pub fn port(&self) -> u16 {
        self.endpoint.port
    }

    pub fn address(&self) -> String {
        self.endpoint().to_string()
    }

    pub fn access_code(&self) -> &str {
        self.access_code.as_str()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

impl LocalEndpointArg {
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    pub fn host(&self) -> &str {
        self.endpoint.host.as_str()
    }

    pub fn port(&self) -> u16 {
        self.endpoint.port
    }

    pub fn address(&self) -> String {
        self.endpoint().to_string()
    }

    pub fn access_code(&self) -> Option<&str> {
        self.access_code.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn into_endpoint(self, access_code: String) -> LocalEndpoint {
        LocalEndpoint {
            endpoint: self.endpoint,
            access_code,
            name: self.name,
        }
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
        write!(formatter, "{}={}", self.id, self.endpoint)
    }
}

impl fmt::Display for LocalEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.endpoint.fmt(formatter)
    }
}

impl fmt::Display for LocalEndpointArg {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.endpoint.fmt(formatter)
    }
}

impl FromStr for LocalEndpointArg {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        parse_local_device_arg(value)
    }
}

pub async fn infer_local_device_id(device: &LocalEndpointArg) -> Result<String> {
    let endpoint = device.endpoint();
    let address = endpoint.to_string();
    let tcp = tokio::time::timeout(
        LOCAL_MQTT_PROBE_TIMEOUT,
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
    )
    .await
    .with_context(|| format!("timed out probing local MQTT TLS at {address}"))?
    .with_context(|| format!("failed to connect to local MQTT TLS at {address}"))?;

    let tls = device_tls::tokio_connector()?;
    let socket = tokio::time::timeout(
        LOCAL_MQTT_PROBE_TIMEOUT,
        tls.connect(endpoint.host.as_str(), tcp),
    )
    .await
    .with_context(|| format!("timed out handshaking local MQTT TLS at {address}"))?
    .with_context(|| format!("failed local MQTT TLS handshake at {address}"))?;

    device_tls::peer_device_id(&socket)
        .with_context(|| format!("local MQTT certificate at {address} did not include a device ID"))
}

fn parse_local_device_arg(value: &str) -> std::result::Result<LocalEndpointArg, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("local device must not be empty".to_owned());
    }
    if value.contains('=') {
        return Err(format!(
            "invalid local device `{value}`: DEVICE_ID= prefix is not supported; use HOST[:PORT][,ACCESS_CODE[,NAME]]"
        ));
    }

    parse_local_endpoint_arg(value, "local device", MQTT_PORT)
}

pub(crate) fn parse_local_endpoint_arg(
    value: &str,
    label: &str,
    default_port: u16,
) -> std::result::Result<LocalEndpointArg, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }

    let fields = value.splitn(3, ',').collect::<Vec<_>>();
    let parsed = parse_endpoint(fields[0].trim(), value, label, default_port)?;
    let access_code = parse_access_code_arg(fields.get(1).copied(), label, value)?;
    let name = optional_field(&fields, 2);

    Ok(LocalEndpointArg {
        endpoint: parsed,
        access_code,
        name,
    })
}

pub(crate) fn parse_access_code_arg(
    access_code: Option<&str>,
    label: &str,
    value: &str,
) -> std::result::Result<Option<String>, String> {
    let access_code = access_code
        .map(str::trim)
        .filter(|access_code| !access_code.is_empty())
        .map(str::to_owned);
    if access_code
        .as_deref()
        .is_some_and(|access_code| !access_code.is_ascii())
    {
        return Err(format!(
            "invalid {label} `{value}`: access code must be ASCII"
        ));
    }
    Ok(access_code)
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
    use super::LocalEndpointArg;

    fn local_device_arg(value: &str) -> LocalEndpointArg {
        value.parse().expect("local device arg should parse")
    }

    #[test]
    fn local_device_parser_accepts_default_mqtt_port() {
        let device = local_device_arg("192.168.1.50,12345678,Office X1");

        assert_eq!(device.endpoint.host, "192.168.1.50");
        assert_eq!(device.endpoint.port, 8883);
        assert_eq!(device.access_code.as_deref(), Some("12345678"));
        assert_eq!(device.name.as_deref(), Some("Office X1"));
    }

    #[test]
    fn local_device_parser_accepts_custom_port() {
        let device = local_device_arg("printer.local:18883,12345678");

        assert_eq!(device.endpoint.host, "printer.local");
        assert_eq!(device.endpoint.port, 18883);
        assert_eq!(device.name, None);
    }

    #[test]
    fn local_device_parser_accepts_missing_access_code() {
        let device = local_device_arg("printer.local");

        assert_eq!(device.endpoint.host, "printer.local");
        assert_eq!(device.access_code, None);
        assert_eq!(device.name, None);

        let device = local_device_arg("printer.local,,Office X1");

        assert_eq!(device.access_code, None);
        assert_eq!(device.name.as_deref(), Some("Office X1"));
    }

    #[test]
    fn local_device_parser_accepts_bracketed_ipv6() {
        let device = local_device_arg("[fe80::1]:18883,12345678");

        assert_eq!(device.endpoint.host, "fe80::1");
        assert_eq!(device.endpoint.port, 18883);
    }

    #[test]
    fn local_device_parser_rejects_device_id_prefix() {
        let error = "printer-a=printer.local:18883,12345678"
            .parse::<LocalEndpointArg>()
            .unwrap_err();

        assert!(error.contains("DEVICE_ID= prefix is not supported"));
    }

    #[test]
    fn local_device_config_accepts_host_only_form() {
        let device = local_device_arg("printer.local:18883,12345678,Office X1");

        assert_eq!(device.endpoint.host, "printer.local");
        assert_eq!(device.endpoint.port, 18883);
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
