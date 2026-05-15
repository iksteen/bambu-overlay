use std::{fmt, str::FromStr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConfig {
    pub id: String,
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

fn parse_device_config(value: &str) -> std::result::Result<DeviceConfig, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("cloud device id must not be empty".to_owned());
    }
    if value.contains(',') {
        return Err(format!(
            "invalid cloud device `{value}`: expected only DEVICE_ID"
        ));
    }

    Ok(DeviceConfig {
        id: value.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn device_config_parser_accepts_id_only() {
        let device: super::DeviceConfig = "printer-a".parse().unwrap();
        assert_eq!(device.id, "printer-a");
    }

    #[test]
    fn device_config_parser_rejects_metadata() {
        let error = "printer-a,12345678"
            .parse::<super::DeviceConfig>()
            .unwrap_err();
        assert!(error.contains("expected only DEVICE_ID"));
    }
}
