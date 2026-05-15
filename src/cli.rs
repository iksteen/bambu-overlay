use std::{collections::HashSet, path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::{
    auth::{default_token_path, load_token, save_token},
    bambu::{BambuClient, LoginResponse, API_BASE, MQTT_HOST},
    cloud::CloudSession,
    local::{Endpoint, LocalEndpointArg, MqttEndpoint},
    monitor::{monitor_mqtt, MonitorConfig},
    video::VideoEndpoint,
    web::{serve, ServerConfig, DEFAULT_HOST, DEFAULT_PORT},
};

#[derive(Parser)]
#[command(name = "bambu-overlay", version, about = "Bambu printer OBS overlay")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Log in and store an access token")]
    Login(LoginArgs),
    #[command(about = "List printers in the token account")]
    Devices(DevicesArgs),
    #[command(about = "Monitor MQTT events for one printer")]
    Mqtt(MqttArgs),
    #[command(about = "Serve an OBS browser overlay page")]
    Serve(ServeArgs),
}

#[derive(Args, Clone)]
struct HttpArgs {
    #[arg(long, default_value = API_BASE)]
    api_base: String,
    #[arg(long, default_value_t = 30.0, value_parser = positive_f64)]
    timeout: f64,
}

#[derive(Args, Clone)]
struct TokenFileArgs {
    #[arg(
        long,
        value_name = "PATH",
        default_value_os_t = default_token_path().to_path_buf(),
        help = "Bambu Cloud token JSON path"
    )]
    token_file: PathBuf,
}

#[derive(Args, Clone)]
struct ServeTokenFileArgs {
    #[arg(
        long,
        value_name = "PATH",
        default_value_os_t = default_token_path().to_path_buf(),
        help = "Bambu Cloud token JSON path",
        help_heading = "Cloud"
    )]
    token_file: PathBuf,
}

#[derive(Args)]
struct LoginArgs {
    #[command(flatten)]
    http: HttpArgs,
    #[command(flatten)]
    token: TokenFileArgs,
    #[arg(long)]
    account: Option<String>,
    #[arg(long)]
    password: Option<String>,
    #[arg(long)]
    code: Option<String>,
}

#[derive(Args)]
struct DevicesArgs {
    #[command(flatten)]
    token: TokenFileArgs,
    #[arg(long, default_value_t = 30.0, value_parser = positive_f64)]
    timeout: f64,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(
        long = "bind",
        value_name = "HOST[:PORT]",
        default_value = DEFAULT_HOST,
        value_parser = parse_bind_endpoint,
        help = "HTTP bind address. Port defaults to 8765",
        help_heading = "Server"
    )]
    bind: Endpoint,
    #[command(flatten)]
    token: ServeTokenFileArgs,
    #[arg(
        long,
        default_value_t = 30.0,
        value_parser = positive_f64,
        help = "Bambu Cloud API timeout in seconds",
        help_heading = "Cloud"
    )]
    timeout: f64,
    #[arg(
        long = "cloud-mqtt",
        value_name = "HOST[:PORT]",
        default_value = MQTT_HOST,
        help = "Bambu Cloud MQTT endpoint. Port defaults to 8883",
        help_heading = "Cloud"
    )]
    cloud_mqtt: MqttEndpoint,
    #[arg(
        long = "cloud-device",
        value_name = "DEVICE_ID",
        value_parser = parse_cloud_device_id,
        help = "Explicit Bambu Cloud MQTT device ID; repeat to add devices. When set, /bind enumeration is skipped",
        help_heading = "Cloud"
    )]
    cloud_devices: Vec<String>,
    #[arg(
        long = "local-device",
        value_name = "HOST[:PORT][,ACCESS_CODE[,NAME]]",
        help = "Printer LAN MQTT device; repeat for multiple printers. Port defaults to 8883. The device ID is inferred from the MQTT certificate. ACCESS_CODE can be provided here or looked up from /bind when needed",
        help_heading = "Local LAN"
    )]
    local_devices: Vec<LocalEndpointArg>,
    #[arg(
        long = "video-device",
        value_name = "HOST[:PORT][,ACCESS_CODE]",
        help = "Printer LAN video endpoint; repeat for multiple printers. Port defaults to 6000. The device ID is inferred from the video certificate and must match a configured cloud or local device. ACCESS_CODE can be provided here or looked up from /bind when needed",
        help_heading = "Local LAN"
    )]
    video_devices: Vec<VideoEndpoint>,
}

#[derive(Args)]
struct MqttArgs {
    #[command(flatten)]
    token: ServeTokenFileArgs,
    #[arg(
        long,
        default_value_t = 30.0,
        value_parser = positive_f64,
        help = "Bambu Cloud API timeout in seconds",
        help_heading = "Cloud"
    )]
    timeout: f64,
    #[arg(
        long = "cloud-mqtt",
        value_name = "HOST[:PORT]",
        default_value = MQTT_HOST,
        help = "Bambu Cloud MQTT endpoint. Port defaults to 8883",
        help_heading = "Cloud"
    )]
    cloud_mqtt: MqttEndpoint,
    #[arg(
        long = "cloud-device",
        value_name = "DEVICE_ID",
        value_parser = parse_cloud_device_id,
        help = "Explicit Bambu Cloud MQTT device ID; repeat to add devices. When set, /bind enumeration is skipped",
        help_heading = "Cloud"
    )]
    cloud_devices: Vec<String>,
    #[arg(
        long = "local-device",
        value_name = "HOST[:PORT][,ACCESS_CODE[,NAME]]",
        help = "Printer LAN MQTT device; repeat for multiple printers. Port defaults to 8883. The device ID is inferred from the MQTT certificate. ACCESS_CODE can be provided here or looked up from /bind when needed",
        help_heading = "Local LAN"
    )]
    local_devices: Vec<LocalEndpointArg>,
    #[arg(
        long = "device",
        value_name = "DEVICE_ID",
        help = "Device ID to monitor. Defaults to the first resolved device",
        help_heading = "Selection"
    )]
    device: Option<String>,
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Login(args) => login(args).await,
        Command::Devices(args) => devices_cmd(args).await,
        Command::Mqtt(args) => mqtt_cmd(args).await,
        Command::Serve(args) => serve_cmd(args).await,
    }
}

async fn login(args: LoginArgs) -> Result<()> {
    let client = client(&args.http)?;
    let account = match args.account {
        Some(account) => account,
        None => prompt("Bambu account email/username: ")?,
    };

    if args.password.is_some() && args.code.is_some() {
        bail!("set only one of --password or --code");
    }

    let mut login_response = if let Some(code) = args.code.as_deref() {
        client.login(&account, None, Some(code)).await?
    } else {
        let password = match args.password {
            Some(password) => password,
            None => rpassword::prompt_password("Bambu password: ")
                .context("failed to read Bambu password")?,
        };
        client.login(&account, Some(&password), None).await?
    };

    if requires_verification_code(&login_response) {
        let code = prompt("Bambu verification code: ")?;
        login_response = client.login(&account, None, Some(&code)).await?;
    }

    let token_path = save_token(
        &login_response,
        Some(args.token.token_file),
        &args.http.api_base,
    )?;
    println!("Saved Bambu access token to {}", token_path.display());
    println!("Run `bambu-overlay serve` to start the overlay.");
    Ok(())
}

async fn devices_cmd(args: DevicesArgs) -> Result<()> {
    let (client, access_token) = token_client(Some(args.token.token_file), args.timeout)?;
    let bound_devices = client.bound_devices(&access_token).await?;

    println!(
        "{:<24}  {:<32}  {:<8}  {:<12}",
        "ID", "NAME", "ONLINE", "ACCESS CODE"
    );
    for device in bound_devices.devices {
        let id = device.id.unwrap_or_else(|| "--".to_owned());
        let name = device.name.unwrap_or_else(|| "--".to_owned());
        let access_code = device.access_code.unwrap_or_else(|| "--".to_owned());
        let online = match device.online {
            Some(true) => "yes",
            Some(false) => "no",
            None => "--",
        };
        println!("{id:<24}  {name:<32}  {online:<8}  {access_code:<12}");
    }
    Ok(())
}

async fn serve_cmd(args: ServeArgs) -> Result<()> {
    validate_devices(&args.cloud_devices)?;
    let config = ServerConfig::from(&args);
    let cloud = optional_token_client(args.token.token_file.clone(), args.timeout)?;
    serve(cloud, config).await
}

async fn mqtt_cmd(args: MqttArgs) -> Result<()> {
    validate_devices(&args.cloud_devices)?;
    let cloud = optional_token_client(args.token.token_file.clone(), args.timeout)?;
    monitor_mqtt(cloud, MonitorConfig::from(&args)).await
}

fn validate_devices(cloud_devices: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    for device_id in cloud_devices {
        if !seen.insert(device_id.as_str()) {
            bail!("--cloud-device includes duplicate device id `{device_id}`");
        }
    }
    Ok(())
}

fn optional_token_client(token_file: PathBuf, timeout: f64) -> Result<Option<CloudSession>> {
    if !token_file.exists() {
        return Ok(None);
    }

    token_client(Some(token_file), timeout).map(|(client, access_token)| {
        Some(CloudSession {
            client,
            access_token,
        })
    })
}

fn token_client(token_file: Option<PathBuf>, timeout: f64) -> Result<(BambuClient, String)> {
    let token_data = load_token(token_file)?;
    let access_token = token_data
        .access_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context("cached token file does not include accessToken")?;
    let api_base = token_data
        .api_base
        .as_deref()
        .filter(|api_base| !api_base.is_empty())
        .unwrap_or(API_BASE);
    let client = BambuClient::new(api_base, Duration::from_secs_f64(timeout))?;
    Ok((client, access_token))
}

fn client(args: &HttpArgs) -> Result<BambuClient> {
    BambuClient::new(&args.api_base, Duration::from_secs_f64(args.timeout))
}

fn requires_verification_code(login_response: &LoginResponse) -> bool {
    login_response
        .login_type
        .as_deref()
        .map(|login_type| login_type.eq_ignore_ascii_case("verifycode"))
        .unwrap_or(false)
}

impl From<&ServeArgs> for ServerConfig {
    fn from(args: &ServeArgs) -> Self {
        Self {
            bind: args.bind.clone(),
            cloud_mqtt: args.cloud_mqtt.clone(),
            local_devices: args.local_devices.clone(),
            cloud_devices: args.cloud_devices.clone(),
            video_endpoints: args.video_devices.clone(),
        }
    }
}

impl From<&MqttArgs> for MonitorConfig {
    fn from(args: &MqttArgs) -> Self {
        Self {
            cloud_mqtt: args.cloud_mqtt.clone(),
            local_devices: args.local_devices.clone(),
            cloud_devices: args.cloud_devices.clone(),
            device: args.device.clone(),
        }
    }
}

fn prompt(label: &str) -> Result<String> {
    use std::io::{self, Write};

    print!("{label}");
    io::stdout().flush().context("failed to flush stdout")?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .context("failed to read stdin")?;
    Ok(value.trim().to_owned())
}

fn positive_f64(value: &str) -> std::result::Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("expected a number, got `{value}`"))?;
    if parsed.is_finite() && parsed > 0.0 {
        Ok(parsed)
    } else {
        Err(format!("expected a positive finite number, got `{value}`"))
    }
}

fn parse_bind_endpoint(value: &str) -> std::result::Result<Endpoint, String> {
    Endpoint::parse_with_default(value, "bind address", DEFAULT_PORT)
}

fn parse_cloud_device_id(value: &str) -> std::result::Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("cloud device id must not be empty".to_owned());
    }
    if value.contains(',') {
        return Err(format!(
            "invalid cloud device `{value}`: expected only DEVICE_ID"
        ));
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_cloud_device_id;

    #[test]
    fn cloud_device_parser_accepts_id_only() {
        assert_eq!(parse_cloud_device_id(" printer-a ").unwrap(), "printer-a");
    }

    #[test]
    fn cloud_device_parser_rejects_metadata() {
        let error = parse_cloud_device_id("printer-a,12345678").unwrap_err();
        assert!(error.contains("expected only DEVICE_ID"));
    }
}
