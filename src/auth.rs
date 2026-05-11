use std::{env, fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::bambu::LoginResponse;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenData {
    pub access_token: Option<String>,
    pub api_base: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedToken<'a> {
    access_token: &'a str,
    refresh_token: Option<&'a str>,
    created_at: String,
    api_base: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
}

pub fn save_token(
    login_response: &LoginResponse,
    token_file: Option<PathBuf>,
    api_base: &str,
) -> Result<PathBuf> {
    let access_token = login_response
        .access_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .context("cannot save token: login response did not include accessToken")?;

    let now = Utc::now();
    let token_data = SavedToken {
        access_token,
        refresh_token: login_response.refresh_token.as_deref(),
        created_at: now.to_rfc3339(),
        api_base,
        expires_in: login_response.expires_in,
        expires_at: login_response
            .expires_in
            .map(|expires_in| (now + Duration::seconds(expires_in)).to_rfc3339()),
    };

    let path = token_path(token_file);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let encoded = serde_json::to_vec_pretty(&token_data)?;
    fs::write(&path, [encoded, b"\n".to_vec()].concat())
        .with_context(|| format!("could not write token file {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not chmod token file {}", path.display()))?;
    }

    Ok(path)
}

pub fn load_token(token_file: Option<PathBuf>) -> Result<TokenData> {
    let path = token_path(token_file);
    if !path.exists() {
        bail!(
            "no cached Bambu token found at {}. Run `bambu-overlay login` first",
            path.display()
        );
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("could not read token file {}", path.display()))?;
    let parsed: TokenData = serde_json::from_str(&text).with_context(|| {
        format!(
            "token file {} is not a valid token JSON object",
            path.display()
        )
    })?;
    Ok(parsed)
}

pub fn token_path(token_file: Option<PathBuf>) -> PathBuf {
    if let Some(token_file) = token_file {
        return token_file;
    }
    if let Ok(xdg_state_home) = env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg_state_home)
            .join("bambu-overlay")
            .join("token.json");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("state")
        .join("bambu-overlay")
        .join("token.json")
}
