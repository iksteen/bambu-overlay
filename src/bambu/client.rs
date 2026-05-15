use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::{Bytes, BytesMut};
use reqwest::{Client, Method};
use serde::{de::DeserializeOwned, Serialize};
use tracing::debug;

use super::{
    error::{api_error_message, response_body_is_error, ApiStatus},
    models::{CurrentPrintResponse, LoginRequest, LoginResponse, TasksResponse, UserPreference},
    USER_AGENT,
};

#[derive(Clone)]
pub struct BambuClient {
    client: Client,
    api_base: String,
    timeout: Duration,
}

pub(crate) struct DownloadedBytes {
    pub(crate) bytes: Bytes,
    pub(crate) content_type: Option<String>,
}

impl BambuClient {
    pub fn new(api_base: impl Into<String>, timeout: Duration) -> Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            client,
            api_base: api_base.into(),
            timeout,
        })
    }

    pub async fn login(
        &self,
        account: &str,
        password: Option<&str>,
        code: Option<&str>,
    ) -> Result<LoginResponse> {
        if password.is_some() == code.is_some() {
            bail!("provide exactly one of password or verification code");
        }

        let body = LoginRequest {
            account,
            password,
            code,
        };

        self.request_json_with_body(Method::POST, "/v1/user-service/user/login", None, &body)
            .await
    }

    pub async fn current_print(&self, access_token: &str) -> Result<CurrentPrintResponse> {
        self.request_json(
            Method::GET,
            "/v1/iot-service/api/user/print",
            Some(access_token),
        )
        .await
    }

    pub async fn bound_devices(&self, access_token: &str) -> Result<CurrentPrintResponse> {
        self.request_json(
            Method::GET,
            "/v1/iot-service/api/user/bind",
            Some(access_token),
        )
        .await
    }

    pub async fn tasks(
        &self,
        access_token: &str,
        limit: usize,
        device_id: Option<&str>,
    ) -> Result<TasksResponse> {
        let path = "/v1/user-service/my/tasks";
        let method = Method::GET;
        let mut request = self
            .client
            .request(method.clone(), self.url(path))
            .bearer_auth(access_token)
            .timeout(self.timeout)
            .query(&[("limit", limit.to_string())]);
        if let Some(device_id) = device_id {
            request = request.query(&[("deviceId", device_id)]);
        }
        send_and_parse(request, &method, path).await
    }

    pub async fn user_preference(&self, access_token: &str) -> Result<UserPreference> {
        self.request_json(
            Method::GET,
            "/v1/design-user-service/my/preference",
            Some(access_token),
        )
        .await
    }

    pub(crate) async fn download_bytes(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> Result<DownloadedBytes> {
        let mut response = self
            .client
            .get(url)
            .timeout(self.timeout)
            .send()
            .await
            .with_context(|| format!("request to `{url}` failed"))?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if !status.is_success() {
            bail!("request to `{url}` returned HTTP {status}");
        }
        if let Some(content_length) = response.content_length() {
            ensure_download_size(content_length, max_bytes, url)?;
        }
        let mut bytes = BytesMut::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("failed to read response body from `{url}`"))?
        {
            ensure_download_size((bytes.len() + chunk.len()) as u64, max_bytes, url)?;
            bytes.extend_from_slice(&chunk);
        }
        Ok(DownloadedBytes {
            bytes: bytes.freeze(),
            content_type,
        })
    }

    async fn request_json<T>(
        &self,
        method: Method,
        path: &str,
        access_token: Option<&str>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let mut request = self
            .client
            .request(method.clone(), self.url(path))
            .timeout(self.timeout);
        if let Some(token) = access_token {
            request = request.bearer_auth(token);
        }
        if path == "/v1/iot-service/api/user/print" {
            request = request.query(&[("force", "true")]);
        }
        send_and_parse(request, &method, path).await
    }

    async fn request_json_with_body<T, B>(
        &self,
        method: Method,
        path: &str,
        access_token: Option<&str>,
        body: &B,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let mut request = self
            .client
            .request(method.clone(), self.url(path))
            .timeout(self.timeout)
            .json(body);
        if let Some(token) = access_token {
            request = request.bearer_auth(token);
        }
        send_and_parse(request, &method, path).await
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.api_base.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

fn ensure_download_size(size: u64, max_bytes: usize, url: &str) -> Result<()> {
    if size > max_bytes as u64 {
        bail!("download from `{url}` exceeds maximum supported size of {max_bytes} bytes");
    }
    Ok(())
}

async fn send_and_parse<T>(
    request: reqwest::RequestBuilder,
    method: &Method,
    path: &str,
) -> Result<T>
where
    T: DeserializeOwned,
{
    let response = request
        .send()
        .await
        .with_context(|| format!("request to Bambu API {method} {path} failed"))?;
    let status = response.status();
    debug!(
        method = %method,
        endpoint = %path,
        status = status.as_u16(),
        "Bambu API endpoint called"
    );
    parse_response(response).await
}

async fn parse_response<T>(response: reqwest::Response) -> Result<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("failed to read response body")?;
    let body = if bytes.is_empty() {
        b"{}".as_slice()
    } else {
        bytes.as_ref()
    };
    let api_status: ApiStatus = serde_json::from_slice(body).with_context(|| {
        let preview = String::from_utf8_lossy(&body[..body.len().min(500)]);
        format!("response was not a valid JSON object: {preview}")
    })?;

    if !status.is_success() {
        return Err(anyhow!(
            "{}",
            api_error_message(Some(status.as_u16()), &api_status)
        ));
    }
    if response_body_is_error(&api_status) {
        return Err(anyhow!("{}", api_error_message(None, &api_status)));
    }
    serde_json::from_slice(body).context("response JSON did not match the expected shape")
}
