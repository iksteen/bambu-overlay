use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::{
    auth::{load_token, save_token},
    bambu::{BambuClient, LoginResponse, API_BASE, MQTT_HOST, MQTT_PORT},
    web::{
        serve, ServerConfig, DEFAULT_HOST, DEFAULT_PORT, DEFAULT_REFRESH_SECONDS,
        DEFAULT_TASK_LIMIT,
    },
};

#[derive(Parser)]
#[command(name = "bambu-overlay", version, about = "Bambu Cloud overlay")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Log in and store an access token")]
    Login(LoginArgs),
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
    #[arg(long)]
    token_file: Option<PathBuf>,
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
struct ServeArgs {
    #[command(flatten)]
    token: TokenFileArgs,
    #[arg(long, default_value_t = 30.0, value_parser = positive_f64)]
    timeout: f64,
    #[arg(long, default_value = DEFAULT_HOST)]
    host: String,
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    #[arg(long, default_value_t = DEFAULT_REFRESH_SECONDS, value_parser = positive_f64)]
    refresh_seconds: f64,
    #[arg(long, default_value_t = DEFAULT_TASK_LIMIT, value_parser = positive_usize)]
    task_limit: usize,
    #[arg(long, default_value = MQTT_HOST)]
    mqtt_host: String,
    #[arg(long, default_value_t = MQTT_PORT)]
    mqtt_port: u16,
    #[arg(long)]
    no_mqtt: bool,
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Login(args) => login(args).await,
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

    let token_path = save_token(&login_response, args.token.token_file, &args.http.api_base)?;
    println!("Saved Bambu access token to {}", token_path.display());
    println!("Run `bambu-overlay serve` to start the overlay.");
    Ok(())
}

async fn serve_cmd(args: ServeArgs) -> Result<()> {
    let token_data = load_token(args.token.token_file.clone())?;
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
    let client = BambuClient::new(api_base, Duration::from_secs_f64(args.timeout))?;
    serve(client, access_token, ServerConfig::from(&args)).await
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
            host: args.host.clone(),
            port: args.port,
            task_limit: args.task_limit,
            refresh_seconds: args.refresh_seconds,
            mqtt_host: args.mqtt_host.clone(),
            mqtt_port: args.mqtt_port,
            no_mqtt: args.no_mqtt,
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

fn positive_usize(value: &str) -> std::result::Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("expected a positive integer, got `{value}`"))?;
    if parsed > 0 {
        Ok(parsed)
    } else {
        Err(format!("expected a positive integer, got `{value}`"))
    }
}
